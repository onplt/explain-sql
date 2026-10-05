//! Connected mode: explainsql runs the query itself, safely, and checks
//! its advice against the database's catalog.

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, thread};

use explainsql_core::advisor::AdviceKind;
use explainsql_core::catalog::Catalog;
use explainsql_core::counterfactual::{self, Answer, Evaluation, Question, Target, Verdict};
use explainsql_core::ir::Plan;
use explainsql_core::scenario::Setting;
use explainsql_core::{Analysis, advisor, catalog, compare};
use explainsql_db::{Database, Error, Mode, Safety, Settings};
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
    let runs = usize::from(cli.runs);
    let viewer = cli.format == Format::Text && !cli.print && interactive();
    if !viewer {
        let mode = if cli.no_analyze {
            Mode::Estimate
        } else {
            Mode::Analyze
        };
        let (plan, mut analysis, catalog) = plan_of(&db, &sql, mode, safety)?;
        if cli.prove {
            prove_all(&db, &sql, &mut analysis, runs, safety);
        }
        if let Some(target) = &cli.why_not {
            let target = Target::parse(target);
            let asking = Asking {
                measure: cli.measure,
                runs,
                safety,
            };
            match why_not(&db, &sql, &plan, &analysis, catalog.as_ref(), &target, asking) {
                Ok(answers) if answers.is_empty() => eprintln!(
                    "explainsql: nothing to ask the planner about: {}",
                    match &target {
                        Target::Name(name) => format!("no sequential scan of {name} in the plan"),
                        _ if cli.measure => "no sequential scan, nested loop or spill takes 10% of the runtime".to_owned(),
                        _ => "no sequential scan or nested loop takes 10% of the runtime (with --measure, spills too)".to_owned(),
                    }
                ),
                Ok(answers) => analysis.record(answers),
                Err(error) => eprintln!("explainsql: cannot ask the planner: {error}"),
            }
        }
        return Ok(emit(&report_for(cli, &plan, &analysis)));
    }

    // The estimated plan at once; EXPLAIN ANALYZE in the background.
    let (plan, analysis, _) = plan_of(&db, &sql, Mode::Estimate, safety)?;
    let hypopg = has_hypopg(&db);
    let canceller = db.canceller();
    let database = db.description().to_owned();
    let (commands, requests) = mpsc::channel::<Command>();
    let (replies, events) = mpsc::channel::<Event>();
    let asking = Asking {
        measure: cli.measure,
        runs,
        safety,
    };
    thread::spawn(move || {
        for command in requests {
            let (sql, estimate_first) = match command {
                Command::Analyze {
                    sql,
                    estimate_first,
                } => (sql, estimate_first),
                Command::WhyNot { sql, plan, node } => {
                    let analysis = explainsql_core::analyze(&plan);
                    let catalog = read_catalog(&db, &plan, &analysis);
                    let target = Target::Node(node);
                    let event = match why_not(
                        &db,
                        &sql,
                        &plan,
                        &analysis,
                        catalog.as_ref(),
                        &target,
                        asking,
                    ) {
                        Ok(answers) if answers.is_empty() => {
                            Event::Failed("Nothing to ask the planner about this node.".to_owned())
                        }
                        Ok(answers) => Event::Answered(answers),
                        Err(error) => Event::Failed(error),
                    };
                    if replies.send(event).is_err() {
                        break;
                    }
                    continue;
                }
                Command::Prove { sql, ddl, measured } => {
                    let event = match prove(&db, &sql, &ddl, measured, runs, safety) {
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
                    Ok((plan, analysis, _)) => {
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
                Ok((plan, analysis, _)) => Event::Plan {
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
        measure: cli.measure,
    };
    explainsql_tui::run_connected(plan, analysis, viewer_options(cli), connection)
        .map_err(|error| format!("cannot open the viewer: {error}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Runs EXPLAIN, analyzes the plan, and checks the advice against the
/// catalog, which it returns when it could be read.
fn plan_of(
    db: &Database,
    sql: &str,
    mode: Mode,
    safety: Safety,
) -> Result<(Plan, Analysis, Option<Catalog>), String> {
    let json = db
        .explain(sql, mode, safety)
        .map_err(|error| error.to_string())?;
    let plan = explainsql_core::parse(&json).map_err(|error| error.to_string())?;
    let mut analysis = explainsql_core::analyze(&plan);
    let catalog = read_catalog(db, &plan, &analysis);
    // Without the catalog, the advice stays as the plan alone gives it.
    if let Some(catalog) = &catalog {
        advisor::refine(&mut analysis.advice, catalog);
    }
    Ok((plan, analysis, catalog))
}

/// What the catalog says about the tables of a plan and its advice.
fn read_catalog(db: &Database, plan: &Plan, analysis: &Analysis) -> Option<Catalog> {
    let (tables, constraints) = catalog::wanted(plan, &analysis.advice);
    db.catalog(&tables, &constraints).ok()
}

/// How to ask the planner why.
#[derive(Clone, Copy)]
struct Asking {
    /// Measure both plans with EXPLAIN ANALYZE rather than estimating.
    measure: bool,
    runs: usize,
    safety: Safety,
}

/// Asks the planner why it chose its plan: plans the statement again under
/// each question's settings and reads the answers. When measuring, both
/// plans are then run, unless the alternative turned out impossible. Every
/// run is rolled back.
fn why_not(
    db: &Database,
    sql: &str,
    plan: &Plan,
    analysis: &Analysis,
    catalog: Option<&Catalog>,
    target: &Target,
    asking: Asking,
) -> Result<Vec<Answer>, String> {
    let questions = counterfactual::questions(plan, analysis, catalog, target, asking.measure);
    let mut chosen: Option<Vec<Plan>> = None;
    let mut answers = Vec::new();
    for question in &questions {
        // Estimated first: cheap, and all it takes when no alternative
        // exists.
        let estimated = match estimate(db, sql, &question.settings, asking.safety) {
            Ok(estimated) => estimated,
            Err((error, text)) => {
                answers.push(unanswered(question, &error, &text, asking));
                continue;
            }
        };
        let with_cost_settings = if question.cost_settings.is_empty() {
            None
        } else {
            estimate(db, sql, &question.cost_settings, asking.safety).ok()
        };
        let evaluation = Evaluation {
            chosen: &[],
            alternative: std::slice::from_ref(&estimated),
            with_cost_settings: with_cost_settings.as_ref(),
            cost_settings_runs: &[],
            catalog,
        };
        let first = counterfactual::answer(plan, analysis, question, &evaluation);
        if !asking.measure || first.verdict == Verdict::Unusable {
            answers.push(first);
            continue;
        }
        // The statement as the planner chooses it, measured once for all
        // questions, as the alternatives are.
        let chosen = match &mut chosen {
            Some(chosen) => chosen,
            None => {
                let runs = db
                    .measure(sql, &[], asking.runs, asking.safety)
                    .map_err(|error| error.to_string())?;
                chosen.insert(parse_all(&runs)?)
            }
        };
        let alternative = match db
            .measure(sql, &question.settings, asking.runs, asking.safety)
            .map_err(|error| (error.clone(), error.to_string()))
            .and_then(|runs| parse_all(&runs).map_err(|text| (Error::Server(text.clone()), text)))
        {
            Ok(alternative) => alternative,
            Err((error, text)) => {
                answers.push(unanswered(question, &error, &text, asking));
                continue;
            }
        };
        // A cost setting is suggested only once the plan it leads to is
        // measured too.
        let cost_settings_runs = match &with_cost_settings {
            Some(with) if counterfactual::measure_cost_settings(question, with) => db
                .measure(sql, &question.cost_settings, asking.runs, asking.safety)
                .ok()
                .and_then(|runs| parse_all(&runs).ok())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        answers.push(counterfactual::answer(
            plan,
            analysis,
            question,
            &Evaluation {
                chosen,
                alternative: &alternative,
                with_cost_settings: with_cost_settings.as_ref(),
                cost_settings_runs: &cost_settings_runs,
                catalog,
            },
        ));
    }
    Ok(answers)
}

/// The estimated plan of the statement under settings.
fn estimate(
    db: &Database,
    sql: &str,
    settings: &[Setting],
    safety: Safety,
) -> Result<Plan, (Error, String)> {
    let json = db
        .explain_with(sql, Mode::Estimate, settings, safety)
        .map_err(|error| (error.clone(), error.to_string()))?;
    explainsql_core::parse(&json)
        .map_err(|error| (Error::Server(error.to_string()), error.to_string()))
}

/// The answer to a question whose alternative did not run to the end: one
/// that runs past the timeout while the planner's choice finished is the
/// slower plan.
fn unanswered(question: &Question, error: &Error, text: &str, asking: Asking) -> Answer {
    match error {
        Error::Timeout(timeout) if asking.measure => {
            counterfactual::timed_out(question, &format!("{} s", timeout.as_secs_f64()))
        }
        _ => counterfactual::failed(question, text),
    }
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
    runs: usize,
    safety: Safety,
) -> Result<compare::Comparison, String> {
    let proof = db
        .prove(sql, ddl, measured, runs, safety)
        .map_err(|error| error.to_string())?;
    Ok(compare::compare_runs(
        &parse_all(&proof.before)?,
        &parse_all(&proof.after)?,
    ))
}

/// Plans as the database returned them.
fn parse_all(plans: &[String]) -> Result<Vec<Plan>, String> {
    plans
        .iter()
        .map(|plan| explainsql_core::parse(plan).map_err(|error| error.to_string()))
        .collect()
}

/// Tests every index suggestion: with HypoPG when it is installed, else by
/// building the index with --allow-ddl.
fn prove_all(db: &Database, sql: &str, analysis: &mut Analysis, runs: usize, safety: Safety) {
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
        match prove(db, sql, &ddl, !hypopg, runs, safety) {
            Ok(comparison) => advisor::verify(advice, comparison, !hypopg),
            Err(error) => eprintln!("explainsql: cannot test {ddl}: {error}"),
        }
    }
}
