//! The interactive viewer: the plan tree with where the time went, the
//! details of each node and the findings, in the terminal.

mod app;
mod theme;
mod ui;

use std::io::{self, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use explainsql_core::Analysis;
use explainsql_core::ir::Plan;
use ratatui::crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers};

pub use app::{App, Key, Live, Outcome};
pub use theme::{Background, Depth, Theme};

/// How the viewer looks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub background: Background,
    /// Detected from the environment when `None`.
    pub depth: Option<Depth>,
}

/// Connected mode: the statement, and the channels to the thread that runs
/// it. The viewer sends [`Command`]s and shows the [`Event`]s that come back.
pub struct Connection {
    /// `user@host:port/dbname`.
    pub database: String,
    pub sql: String,
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
    /// Stops the running statement, from the viewer's thread.
    pub cancel: Box<dyn Fn() + Send>,
    /// The plan shown first was measured, not estimated.
    pub measured: bool,
    /// Start an EXPLAIN ANALYZE as soon as the viewer opens.
    pub analyze_now: bool,
    /// HypoPG is installed: suggestions can be tested without building them.
    pub hypopg: bool,
    /// Suggestions may be built, in a transaction that is rolled back.
    pub allow_ddl: bool,
    /// `y` measures the alternatives rather than only estimating them.
    pub measure: bool,
}

/// What the viewer asks the connection to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run the statement with EXPLAIN ANALYZE; first send the estimated plan
    /// when `estimate_first` (after an edit).
    Analyze { sql: String, estimate_first: bool },
    /// Test an index: estimated with HypoPG, or `measured` with the index
    /// built and rolled back.
    Prove {
        sql: String,
        ddl: String,
        measured: bool,
    },
    /// Ask the planner why it chose what it chose for a node of the plan.
    WhyNot {
        sql: String,
        plan: Box<Plan>,
        node: explainsql_core::ir::NodeId,
    },
}

/// What comes back.
pub enum Event {
    Plan {
        plan: Box<Plan>,
        analysis: Box<Analysis>,
        measured: bool,
    },
    /// The test of the suggestion whose statement is `ddl`.
    Proved {
        ddl: String,
        comparison: Box<explainsql_core::compare::Comparison>,
        measured: bool,
    },
    /// What the planner said when asked again.
    Answered(Vec<explainsql_core::counterfactual::Answer>),
    Failed(String),
}

/// Shows a plan until the user quits. Keys are read from the terminal even
/// when the plan came from standard input.
pub fn run(plan: Plan, analysis: Analysis, options: Options) -> io::Result<()> {
    run_with(App::new(plan, analysis), options, None)
}

/// Shows a plan from a database, with `r` to run the statement again, `e`
/// to edit it and `Esc` to cancel a run.
pub fn run_connected(
    plan: Plan,
    analysis: Analysis,
    options: Options,
    connection: Connection,
) -> io::Result<()> {
    let mut app = App::new(plan, analysis);
    app.live = Some(Live {
        database: connection.database.clone(),
        sql: connection.sql.clone(),
        measured: connection.measured,
        running: None,
        task: String::new(),
        hypopg: connection.hypopg,
        allow_ddl: connection.allow_ddl,
        measure: connection.measure,
    });
    run_with(app, options, Some(connection))
}

fn run_with(mut app: App, options: Options, connection: Option<Connection>) -> io::Result<()> {
    let theme = Theme::new(
        options.background,
        options.depth.unwrap_or_else(Depth::detect),
    );
    if let Some(connection) = &connection {
        if connection.analyze_now {
            start(&mut app, connection, false);
        }
    }
    // Restores the terminal on panic as well.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, &mut app, &theme, connection.as_ref());
    ratatui::try_restore()?;
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    theme: &Theme,
    connection: Option<&Connection>,
) -> io::Result<()> {
    loop {
        if let Some(connection) = connection {
            while let Ok(event) = connection.events.try_recv() {
                receive(app, event);
            }
        }
        terminal.draw(|frame| ui::draw(frame, app, theme))?;
        // While a statement runs, wake up to show the elapsed time and the
        // result.
        let running = app.live.as_ref().is_some_and(|live| live.running.is_some());
        if connection.is_some()
            && !event::poll(Duration::from_millis(if running { 100 } else { 250 }))?
        {
            continue;
        }
        let event::Event::Key(key) = event::read()? else {
            // Resizes and other events just redraw.
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Ok(());
        }
        let key = match key.code {
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Tab | KeyCode::BackTab => Key::Tab,
            KeyCode::Backspace => Key::Backspace,
            _ => continue,
        };
        let page = app.tree_height.saturating_sub(1);
        match (app.handle(key, page), connection) {
            (Outcome::Quit, _) => return Ok(()),
            (Outcome::Copy(text), _) => copy(&text)?,
            (Outcome::Run, Some(connection)) => start(app, connection, false),
            (Outcome::Prove { ddl, measured }, Some(connection)) => {
                let Some(live) = &mut app.live else { continue };
                let command = Command::Prove {
                    sql: live.sql.clone(),
                    ddl,
                    measured,
                };
                if connection.commands.send(command).is_ok() {
                    live.running = Some(Instant::now());
                    live.task = if measured {
                        "Building the index in a rolled-back transaction".to_owned()
                    } else {
                        "Testing a hypothetical index (HypoPG)".to_owned()
                    };
                }
            }
            (Outcome::WhyNot { node }, Some(connection)) => {
                let Some(live) = &mut app.live else { continue };
                let command = Command::WhyNot {
                    sql: live.sql.clone(),
                    plan: Box::new(app.plan.clone()),
                    node,
                };
                if connection.commands.send(command).is_ok() {
                    live.running = Some(Instant::now());
                    live.task = if live.measure {
                        "Measuring the planner's choice and the alternative".to_owned()
                    } else {
                        "Asking the planner".to_owned()
                    };
                }
            }
            (Outcome::Edit, Some(connection)) => {
                let sql = app
                    .live
                    .as_ref()
                    .map(|live| live.sql.clone())
                    .unwrap_or_default();
                ratatui::try_restore()?;
                let edited = edit(&sql);
                *terminal = ratatui::try_init()?;
                match edited {
                    Ok(Some(sql)) => {
                        if let Some(live) = &mut app.live {
                            live.sql = sql;
                        }
                        start(app, connection, true);
                    }
                    Ok(None) => app.message = Some("The statement is unchanged.".to_owned()),
                    Err(error) => app.message = Some(format!("Cannot edit: {error}")),
                }
            }
            (Outcome::Cancel, Some(connection)) => {
                (connection.cancel)();
                app.message = Some("Cancelling…".to_owned());
            }
            _ => {}
        }
    }
}

/// Asks the connection for an EXPLAIN ANALYZE of the current statement.
fn start(app: &mut App, connection: &Connection, estimate_first: bool) {
    let Some(live) = &mut app.live else {
        return;
    };
    let command = Command::Analyze {
        sql: live.sql.clone(),
        estimate_first,
    };
    if connection.commands.send(command).is_ok() {
        live.running = Some(Instant::now());
        live.task = "Running EXPLAIN ANALYZE".to_owned();
    } else {
        app.message = Some("The connection is closed.".to_owned());
    }
}

fn receive(app: &mut App, event: Event) {
    match event {
        Event::Plan {
            plan,
            analysis,
            measured,
        } => {
            // A measured plan after a measured plan: how the change did.
            let previous = app.live.as_ref().is_some_and(|live| live.measured) && measured;
            let comparison = previous.then(|| explainsql_core::compare::compare(&app.plan, &plan));
            app.replace(*plan, *analysis);
            if let Some(live) = &mut app.live {
                live.measured = measured;
                if measured {
                    live.running = None;
                }
            }
            if let Some(comparison) = comparison {
                app.message = Some(format!(
                    "Compared with the previous run: {}.",
                    comparison.details()
                ));
            }
        }
        Event::Proved {
            ddl,
            comparison,
            measured,
        } => {
            if let Some(live) = &mut app.live {
                live.running = None;
            }
            let summary = comparison.details();
            let advice = app.analysis.advice.iter_mut().find(|advice| {
                matches!(&advice.kind, explainsql_core::advisor::AdviceKind::Index { ddl: other, .. } if *other == ddl)
            });
            if let Some(advice) = advice {
                explainsql_core::advisor::verify(advice, *comparison, measured);
            }
            app.message = Some(format!("Tested: {summary}."));
        }
        Event::Answered(answers) => {
            if let Some(live) = &mut app.live {
                live.running = None;
            }
            app.message = answers.first().map(|answer| {
                format!(
                    "{}: {}. The details show why (J and K scroll).",
                    answer.verdict.label(),
                    answer.verdict.describe()
                )
            });
            app.analysis.record(answers);
        }
        Event::Failed(error) => {
            if let Some(live) = &mut app.live {
                live.running = None;
            }
            app.message = Some(error);
        }
    }
}

/// Opens the statement in `$VISUAL` or `$EDITOR`; `None` when it comes back
/// unchanged or empty.
fn edit(sql: &str) -> io::Result<Option<String>> {
    let path = std::env::temp_dir().join(format!("explainsql-{}.sql", std::process::id()));
    std::fs::write(&path, format!("{sql}\n"))?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad" } else { "vi" }.to_owned());
    let status = if cfg!(windows) {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .status()
    };
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if !status?.success() {
        return Err(io::Error::other(format!("{editor} failed")));
    }
    let edited = edited?.trim().to_owned();
    Ok((!edited.is_empty() && edited != sql.trim()).then_some(edited))
}

/// Puts text on the clipboard with the OSC 52 escape sequence, which
/// terminals support over SSH and inside tmux.
fn copy(text: &str) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &byte)| n | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Draws one frame into a buffer, for tests and benchmarks.
pub fn render(app: &mut App, theme: &Theme, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("a test backend never fails");
    terminal
        .draw(|frame| ui::draw(frame, app, theme))
        .expect("a test backend never fails");
    terminal.backend().buffer().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the planner said shows in the status line and the details, and
    /// ends the run.
    #[test]
    fn receives_answers() {
        use explainsql_core::counterfactual::{self, Evaluation, Target};
        use explainsql_core::ir::NodeId;
        let plan = explainsql_core::parse(
            "Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)\n  Filter: (customer_id = 4242)\n  Rows Removed by Filter: 199990\nExecution Time: 11.900 ms",
        )
        .unwrap();
        let analysis = explainsql_core::analyze(&plan);
        let question =
            counterfactual::questions(&plan, &analysis, None, &Target::Node(NodeId(0)), false)
                .remove(0);
        let alternative = explainsql_core::parse(
            "Seq Scan on orders  (cost=10000000000.00..10000004917.00 rows=10 width=64)\n  Filter: (customer_id = 4242)",
        )
        .unwrap();
        let answer = counterfactual::answer(
            &plan,
            &analysis,
            &question,
            &Evaluation {
                chosen: &[],
                alternative: std::slice::from_ref(&alternative),
                with_cost_settings: None,
                cost_settings_runs: &[],
                catalog: None,
            },
        );
        let mut app = App::new(plan, analysis);
        app.live = Some(Live {
            database: "db".to_owned(),
            sql: "SELECT".to_owned(),
            measured: true,
            running: Some(Instant::now()),
            task: "Asking the planner".to_owned(),
            hypopg: false,
            allow_ddl: false,
            measure: false,
        });
        receive(&mut app, Event::Answered(vec![answer.clone()]));
        assert!(app.live.as_ref().unwrap().running.is_none());
        assert_eq!(
            app.message.as_deref(),
            Some(
                "UNUSABLE: no index can serve the condition. The details show why (J and K scroll)."
            )
        );
        assert_eq!(app.analysis.counterfactuals, std::slice::from_ref(&answer));
        // Asked again: the new answer replaces the old one.
        receive(&mut app, Event::Answered(vec![answer]));
        assert_eq!(app.analysis.counterfactuals.len(), 1);
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
