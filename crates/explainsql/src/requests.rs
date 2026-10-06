//! `explainsql requests`: the statements of server logs grouped into
//! requests, and the loops among them (N+1). With -d, each loop's batched
//! statement is measured against its runs, every run rolled back, and the
//! foreign key behind it named.

use std::process::ExitCode;
use std::time::Duration;

use explainsql_core::ir::Plan;
use explainsql_core::pg::{LoggedStatement, ParseError, parameter_values};
use explainsql_core::report;
use explainsql_core::requests::{self, Loop, LoopKind, Options, Profile};
use explainsql_db::{Cache, Database, Mode, Safety, Settings};

use crate::{Format, RequestsArgs, emit, read_input, use_color};

/// The most runs of a loop measured one by one; the figures of more are
/// scaled from these.
const MEASURED_RUNS: usize = 20;
/// The round trips timed to take their median.
const ROUND_TRIPS: usize = 9;

pub fn run(args: &RequestsArgs) -> ExitCode {
    match try_run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_run(args: &RequestsArgs) -> Result<ExitCode, String> {
    let mut statements: Vec<LoggedStatement> = Vec::new();
    for file in &args.files {
        let text = read_input(Some(file), false).map_err(|error| error.to_string())?;
        let mut found = read(&text).map_err(|error| format!("{file}: {error}"))?;
        if args.files.len() > 1 {
            for statement in &mut found {
                statement.meta.file = Some(file.clone());
            }
        }
        statements.extend(found);
    }
    let options = Options {
        min_runs: usize::from(args.min_runs),
        gap: args.gap,
    };
    let mut profile = requests::profile(&statements, options);
    if let Some(dbname) = args.dbname.as_deref() {
        prove(&mut profile, dbname, args)?;
    }
    let limit = usize::from(args.limit);
    Ok(emit(&match args.format {
        Format::Text => report::requests_text(&profile, limit, options, use_color(args.color)),
        Format::Md => report::requests_markdown(&profile, limit, options),
        Format::Json => report::requests_json(&statements, &profile),
    }))
}

/// The statements a log says ran: from statement logging, or else from
/// auto_explain's entries, which carry each statement too.
fn read(text: &str) -> Result<Vec<LoggedStatement>, String> {
    match explainsql_core::pg::parse_statements(text) {
        Ok(statements) => return Ok(statements),
        Err(ParseError::Empty) => return Err("the log is empty".to_owned()),
        Err(_) => {}
    }
    let Ok((entries, _)) = explainsql_core::parse_log(text) else {
        return Err(
            "no statement in it: log them with log_min_duration_statement = 0, or log_transaction_sample_rate to log whole transactions"
                .to_owned(),
        );
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let text = entry.plan.summary.query_text?;
            Some(LoggedStatement {
                meta: entry.meta,
                text,
                prepared: None,
                parameters: entry
                    .parameters
                    .as_deref()
                    .map(parameter_values)
                    .unwrap_or_default(),
            })
        })
        .collect())
}

/// Measures the batched statement of each loop shown against its runs, and
/// looks up the foreign key behind it.
fn prove(profile: &mut Profile, dbname: &str, args: &RequestsArgs) -> Result<(), String> {
    let settings = Settings::resolve(Some(dbname))?;
    let db = Database::connect(&settings).map_err(|error| error.to_string())?;
    let safety = Safety {
        allow_dml: args.allow_dml,
        allow_ddl: false,
        timeout: Duration::from_secs(args.timeout.max(1)),
    };
    let round_trip = db
        .round_trip(ROUND_TRIPS)
        .ok()
        .map(|time| time.as_secs_f64() * 1000.0);
    let limit = usize::from(args.limit);
    for (number, item) in profile.loops.iter_mut().take(limit).enumerate() {
        if item.kind != LoopKind::Loop || item.batched.is_err() || item.values.is_empty() {
            continue;
        }
        eprintln!("explainsql: measuring loop {}…", number + 1);
        if let Err(error) = prove_loop(&db, item, round_trip, safety, args) {
            item.notes.push(format!("not measured: {error}"));
        }
    }
    Ok(())
}

fn prove_loop(
    db: &Database,
    item: &mut Loop,
    round_trip: Option<f64>,
    safety: Safety,
    args: &RequestsArgs,
) -> Result<(), String> {
    let types = db
        .parameter_types(&item.single, safety)
        .map_err(|error| error.to_string())?;
    reference(db, item, &types, safety);
    let batched = requests::batch(&item.single, &item.varying, &types)?;

    // One run's values, each varying parameter an array of all the runs'.
    let mut values = item.values[0].clone();
    let mut distinct = 0;
    for &number in &item.varying {
        let all: Vec<Option<&str>> = item
            .values
            .iter()
            .map(|run| run.get(number - 1).and_then(Option::as_deref))
            .collect();
        let mut seen: Vec<&str> = all.iter().flatten().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        distinct = distinct.max(seen.len());
        if let Some(slot) = values.get_mut(number - 1) {
            *slot = Some(requests::array_literal(&all));
        }
    }
    // The batched statement first: its warm-up run reads what the runs
    // read, so both sides find it cached.
    let runs = usize::from(args.runs);
    let batched_plans = parse(
        db.measure_prepared(&batched.sql, Cache::Custom, &values, runs, safety)
            .map_err(|error| error.to_string())?,
    )?;
    let mut single_plans = Vec::new();
    for run in item.values.iter().take(MEASURED_RUNS) {
        let json = db
            .explain_prepared(&item.single, Cache::Custom, run, &[], Mode::Analyze, safety)
            .map_err(|error| error.to_string())?;
        single_plans.push(explainsql_core::parse(&json).map_err(|error| error.to_string())?);
    }
    item.proof = Some(requests::proof(
        batched.sql,
        item.values.len(),
        distinct,
        &single_plans,
        &batched_plans,
        round_trip,
    ));
    Ok(())
}

/// The foreign key behind a loop: from the column its varying value is
/// compared with, as the generic plan says, and the advice for it.
fn reference(db: &Database, item: &mut Loop, types: &[String], safety: Safety) {
    let Some(&number) = item.varying.first() else {
        return;
    };
    let Ok(json) = db.explain_prepared(
        &item.single,
        Cache::Generic,
        &item.values[0],
        &[],
        Mode::Estimate,
        safety,
    ) else {
        return;
    };
    let Ok(plan) = explainsql_core::parse(&json) else {
        return;
    };
    let parameters = explainsql_core::params::parameters(&plan, &item.single, types);
    let Some(column) = parameters
        .get(number - 1)
        .and_then(|parameter| parameter.column.as_ref())
    else {
        return;
    };
    let Ok(keys) = db.references(column.schema.as_deref(), &column.table, &column.column) else {
        return;
    };
    item.reference =
        requests::reference(&column.table, &column.column, &keys, item.after.as_deref());
    if item.reference.is_some() {
        item.advice = requests::advice(item);
    }
}

fn parse(plans: Vec<String>) -> Result<Vec<Plan>, String> {
    plans
        .iter()
        .map(|plan| explainsql_core::parse(plan).map_err(|error| error.to_string()))
        .collect()
}
