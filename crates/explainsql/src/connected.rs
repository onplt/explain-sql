//! Connected mode: explainsql runs the query itself, safely, and checks
//! its advice against the database's catalog.

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, thread};

use explainsql_core::ir::Plan;
use explainsql_core::{Analysis, advisor, catalog};
use explainsql_db::{Database, Mode, Safety, Settings};
use explainsql_tui::{Command, Connection, Event};

use crate::{Cli, Format, emit, interactive, report_for, viewer_options};

pub fn run(cli: &Cli) -> ExitCode {
    match try_run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_run(cli: &Cli) -> Result<ExitCode, String> {
    let sql = match (&cli.query_file, &cli.command) {
        (Some(path), _) => fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?,
        (None, Some(sql)) => sql.clone(),
        (None, None) => unreachable!("connected mode needs a query"),
    };
    let settings = Settings::resolve(cli.dbname.as_deref())?;
    let db = Database::connect(&settings).map_err(|error| error.to_string())?;
    let safety = Safety {
        allow_dml: cli.allow_dml,
        timeout: Duration::from_secs(cli.timeout.max(1)),
    };
    let viewer = cli.format == Format::Text && !cli.print && interactive();
    if !viewer {
        let mode = if cli.no_analyze {
            Mode::Estimate
        } else {
            Mode::Analyze
        };
        let (plan, analysis) = plan_of(&db, &sql, mode, safety)?;
        return Ok(emit(&report_for(cli, &plan, &analysis)));
    }

    // The estimated plan at once; EXPLAIN ANALYZE in the background.
    let (plan, analysis) = plan_of(&db, &sql, Mode::Estimate, safety)?;
    let canceller = db.canceller();
    let database = db.description().to_owned();
    let (commands, requests) = mpsc::channel::<Command>();
    let (replies, events) = mpsc::channel::<Event>();
    thread::spawn(move || {
        for command in requests {
            let Command::Analyze {
                sql,
                estimate_first,
            } = command;
            if estimate_first {
                match plan_of(&db, &sql, Mode::Estimate, safety) {
                    Ok((plan, analysis)) => {
                        let _ = replies.send(Event::Plan {
                            plan: Box::new(plan),
                            analysis: Box::new(analysis),
                            measured: false,
                        });
                    }
                    Err(error) => {
                        let _ = replies.send(Event::Failed(error));
                        continue;
                    }
                }
            }
            let event = match plan_of(&db, &sql, Mode::Analyze, safety) {
                Ok((plan, analysis)) => Event::Plan {
                    plan: Box::new(plan),
                    analysis: Box::new(analysis),
                    measured: true,
                },
                Err(error) => Event::Failed(error),
            };
            if replies.send(event).is_err() {
                break;
            }
        }
    });
    let connection = Connection {
        database,
        sql,
        commands,
        events,
        cancel: Box::new(move || canceller.cancel()),
        measured: false,
        analyze_now: !cli.no_analyze,
    };
    explainsql_tui::run_connected(plan, analysis, viewer_options(cli), connection)
        .map_err(|error| format!("cannot open the viewer: {error}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Runs EXPLAIN, analyzes the plan, and checks the advice against the
/// catalog.
fn plan_of(
    db: &Database,
    sql: &str,
    mode: Mode,
    safety: Safety,
) -> Result<(Plan, Analysis), String> {
    let json = db
        .explain(sql, mode, safety)
        .map_err(|error| error.to_string())?;
    let plan = explainsql_core::parse(&json).map_err(|error| error.to_string())?;
    let mut analysis = explainsql_core::analyze(&plan);
    let (tables, constraints) = catalog::wanted(&plan, &analysis.advice);
    // Without the catalog, the advice stays as the plan alone gives it.
    if let Ok(catalog) = db.catalog(&tables, &constraints) {
        advisor::refine(&mut analysis.advice, &catalog);
    }
    Ok((plan, analysis))
}
