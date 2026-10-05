//! Connected mode: explainsql runs the query itself, safely, and checks
//! its advice against the database's catalog.

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, thread};

use explainsql_core::advisor::AdviceKind;
use explainsql_core::ir::Plan;
use explainsql_core::{Analysis, advisor, catalog, compare};
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
        allow_ddl: cli.allow_ddl,
        timeout: Duration::from_secs(cli.timeout.max(1)),
    };
    let viewer = cli.format == Format::Text && !cli.print && interactive();
    if !viewer {
        let mode = if cli.no_analyze {
            Mode::Estimate
        } else {
            Mode::Analyze
        };
        let (plan, mut analysis) = plan_of(&db, &sql, mode, safety)?;
        if cli.prove {
            prove_all(&db, &sql, &mut analysis, safety);
        }
        return Ok(emit(&report_for(cli, &plan, &analysis)));
    }

    // The estimated plan at once; EXPLAIN ANALYZE in the background.
    let (plan, analysis) = plan_of(&db, &sql, Mode::Estimate, safety)?;
    let hypopg = has_hypopg(&db);
    let canceller = db.canceller();
    let database = db.description().to_owned();
    let (commands, requests) = mpsc::channel::<Command>();
    let (replies, events) = mpsc::channel::<Event>();
    thread::spawn(move || {
        for command in requests {
            let (sql, estimate_first) = match command {
                Command::Analyze {
                    sql,
                    estimate_first,
                } => (sql, estimate_first),
                Command::Prove { sql, ddl, measured } => {
                    let event = match prove(&db, &sql, &ddl, measured, safety) {
                        Ok(comparison) => Event::Proved {
                            ddl,
                            comparison: Box::new(comparison),
                            measured,
                        },
                        Err(error) => Event::Failed(error),
                    };
                    if replies.send(event).is_err() {
                        break;
                    }
                    continue;
                }
            };
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
        hypopg,
        allow_ddl: cli.allow_ddl,
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

fn has_hypopg(db: &Database) -> bool {
    db.catalog(&[], &[])
        .is_ok_and(|catalog| catalog.extensions.iter().any(|name| name == "hypopg"))
}

/// The statement without and with an index, compared.
fn prove(
    db: &Database,
    sql: &str,
    ddl: &str,
    measured: bool,
    safety: Safety,
) -> Result<compare::Comparison, String> {
    let proof = db
        .prove(sql, ddl, measured, safety)
        .map_err(|error| error.to_string())?;
    let before = explainsql_core::parse(&proof.before).map_err(|error| error.to_string())?;
    let after = explainsql_core::parse(&proof.after).map_err(|error| error.to_string())?;
    Ok(compare::compare(&before, &after))
}

/// Tests every index suggestion: with HypoPG when it is installed, else by
/// building the index with --allow-ddl.
fn prove_all(db: &Database, sql: &str, analysis: &mut Analysis, safety: Safety) {
    let hypopg = has_hypopg(db);
    if !hypopg && !safety.allow_ddl {
        eprintln!(
            "explainsql: cannot test the suggestions: install HypoPG (CREATE EXTENSION hypopg) or pass --allow-ddl"
        );
        return;
    }
    for advice in &mut analysis.advice {
        let AdviceKind::Index { ddl, .. } = &advice.kind else {
            continue;
        };
        let ddl = ddl.clone();
        match prove(db, sql, &ddl, !hypopg, safety) {
            Ok(comparison) => advisor::verify(advice, comparison, !hypopg),
            Err(error) => eprintln!("explainsql: cannot test {ddl}: {error}"),
        }
    }
}
