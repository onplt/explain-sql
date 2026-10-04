//! The interactive viewer: the plan tree with where the time went, the
//! details of each node and the findings, in the terminal.

mod app;
mod theme;
mod ui;

use std::io;

use explainsql_core::Analysis;
use explainsql_core::ir::Plan;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

pub use app::{App, Key, Outcome};
pub use theme::{Background, Depth, Theme};

/// How the viewer looks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub background: Background,
    /// Detected from the environment when `None`.
    pub depth: Option<Depth>,
}

/// Shows a plan until the user quits. Keys are read from the terminal even
/// when the plan came from standard input.
pub fn run(plan: Plan, analysis: Analysis, options: Options) -> io::Result<()> {
    let theme = Theme::new(
        options.background,
        options.depth.unwrap_or_else(Depth::detect),
    );
    let mut app = App::new(plan, analysis);
    // Restores the terminal on panic as well.
    let mut terminal = ratatui::try_init()?;
    let result = (|| loop {
        terminal.draw(|frame| ui::draw(frame, &mut app, &theme))?;
        let Event::Key(key) = event::read()? else {
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
        if app.handle(key, page) == Outcome::Quit {
            return Ok(());
        }
    })();
    ratatui::try_restore()?;
    result
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
