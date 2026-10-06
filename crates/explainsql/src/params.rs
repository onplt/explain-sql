//! `--params`: how the plan of a statement with parameters depends on
//! their values. The statement is prepared and executed as an application
//! runs it, with values from the columns' statistics or common row counts,
//! once for a custom plan and once for the generic plan, and with
//! `--measure` both plans run where they differ. Every run is rolled back
//! and every prepared statement deallocated.

use explainsql_core::compare;
use explainsql_core::ir::Plan;
use explainsql_core::params::{self, Sample, Sensitivity, Trial, Tried};
use explainsql_core::scenario::Setting;
use explainsql_db::{Cache, Database, Error, Mode, Safety};

/// How to try the values.
#[derive(Clone, Copy)]
pub(crate) struct Trying {
    /// Run both plans where they differ, rather than only estimate them.
    pub measure: bool,
    pub runs: usize,
    pub safety: Safety,
}

/// The plan to show, the generic plan, and how the statement's plan
/// depends on its parameters' values. `given` holds values the user gave,
/// by parameter number.
pub(crate) fn sensitivity(
    db: &Database,
    sql: &str,
    given: &[(usize, String)],
    trying: Trying,
) -> Result<(Plan, Sensitivity), String> {
    let placeholders = params::placeholders(sql);
    if placeholders.count == 0 {
        return Err(
            "the statement takes no parameters: --params is for statements with $1, $2, … or JDBC's ? placeholders"
                .to_owned(),
        );
    }
    let sql = placeholders.sql.as_str();
    let safety = trying.safety;
    let types = db
        .parameter_types(sql, safety)
        .map_err(|error| error.to_string())?;
    let mut given_values = vec![None; types.len()];
    for (number, value) in given {
        match given_values.get_mut(number.wrapping_sub(1)) {
            Some(slot) => *slot = Some(value.clone()),
            None => {
                return Err(format!(
                    "--bind {number}: the statement takes {} parameter{}",
                    types.len(),
                    if types.len() == 1 { "" } else { "s" }
                ));
            }
        }
    }

    // What each parameter is compared with, from the generic plan with
    // every partition in it: pruning would leave out the partitions the
    // values rule out, NULL all of them.
    let nulls = vec![None; types.len()];
    let unpruned = estimate(
        db,
        sql,
        Cache::Generic,
        &nulls,
        &[Setting::new("enable_partition_pruning", "off")],
        safety,
    )?;
    let mut parameters = params::parameters(&unpruned, sql, &types);
    let samples: Vec<Vec<Sample>> = parameters
        .iter_mut()
        .map(|parameter| {
            if let Some(clause) = parameter.clause {
                return params::row_counts(clause);
            }
            let Some(column) = parameter.column.as_mut() else {
                return Vec::new();
            };
            match db.column_stats(column.schema.as_deref(), &column.table, &column.column) {
                Ok(Some(stats)) => {
                    if stats.table != column.table {
                        column.partitioned = Some(stats.table.clone());
                    }
                    params::samples(&stats, &column.operator)
                }
                _ => Vec::new(),
            }
        })
        .collect();
    params::hold(&mut parameters, &samples, &given_values);
    let held: Vec<Option<String>> = parameters
        .iter()
        .map(|parameter| parameter.held.clone())
        .collect();
    let generic = estimate(db, sql, Cache::Generic, &held, &[], safety)?;

    let mut tried = Vec::new();
    let mut generic_runs = Vec::new();
    let mut notes = Vec::new();
    for trial in params::trials(&parameters, &samples) {
        let planned =
            estimate(db, sql, Cache::Custom, &trial.values, &[], safety).and_then(|custom| {
                Ok((
                    custom,
                    estimate(db, sql, Cache::Generic, &trial.values, &[], safety)?,
                ))
            });
        let (custom, generic_with) = match planned {
            Ok(planned) => planned,
            Err(error) => {
                notes.push(format!(
                    "{} could not be tried: {error}",
                    values(&trial, &parameters)
                ));
                continue;
            }
        };
        let mut item = Tried {
            trial,
            custom,
            generic: generic_with,
            measured: None,
            timed_out: None,
        };
        let mut runs = None;
        // Measured where the plans differ, unless the values select no row.
        if trying.measure
            && !params::same_plan(&item.custom, &item.generic)
            && !params::proves_empty(&item.custom)
        {
            match measure(db, sql, &item.trial.values, trying) {
                Ok(Measured::Both { custom, generic }) => {
                    item.measured = Some(compare::compare_runs(&custom, &generic));
                    runs = Some(generic);
                }
                Ok(Measured::GenericTimedOut { custom, timeout }) => {
                    item.measured = Some(compare::compare_runs(&custom, &[]));
                    item.timed_out = Some(timeout);
                }
                Err(error) => notes.push(format!(
                    "{} could not be measured: {error}",
                    values(&item.trial, &parameters)
                )),
            }
        }
        tried.push(item);
        generic_runs.push(runs);
    }

    let mut sensitivity = params::sensitivity(&generic, parameters, tried, placeholders.converted);
    sensitivity.notes.extend(notes);
    // Measured, the report shows the generic plan with the values it does
    // worst with.
    let worst = sensitivity
        .worst()
        .and_then(|index| Some((index, generic_runs.get_mut(index)?.take()?)));
    let plan = match worst {
        Some((index, runs)) => {
            let values = sensitivity.rows[index].values.clone();
            sensitivity.show_measured(&values, true);
            median(runs)
        }
        None => generic,
    };
    Ok((plan, sensitivity))
}

/// Both plans measured with a trial's values, or the custom plan alone
/// when the generic plan ran past the timeout.
enum Measured {
    Both {
        custom: Vec<Plan>,
        generic: Vec<Plan>,
    },
    GenericTimedOut {
        custom: Vec<Plan>,
        timeout: String,
    },
}

fn measure(
    db: &Database,
    sql: &str,
    values: &[Option<String>],
    trying: Trying,
) -> Result<Measured, String> {
    let custom = parse_all(
        &db.measure_prepared(sql, Cache::Custom, values, trying.runs, trying.safety)
            .map_err(|error| error.to_string())?,
    )?;
    match db.measure_prepared(sql, Cache::Generic, values, trying.runs, trying.safety) {
        Ok(generic) => Ok(Measured::Both {
            custom,
            generic: parse_all(&generic)?,
        }),
        Err(Error::Timeout(timeout)) => Ok(Measured::GenericTimedOut {
            custom,
            timeout: format!("{} s", timeout.as_secs_f64()),
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// The estimated plan of the prepared statement.
fn estimate(
    db: &Database,
    sql: &str,
    cache: Cache,
    values: &[Option<String>],
    settings: &[Setting],
    safety: Safety,
) -> Result<Plan, String> {
    let json = db
        .explain_prepared(sql, cache, values, settings, Mode::Estimate, safety)
        .map_err(|error| error.to_string())?;
    explainsql_core::parse(&json).map_err(|error| error.to_string())
}

fn parse_all(plans: &[String]) -> Result<Vec<Plan>, String> {
    plans
        .iter()
        .map(|plan| explainsql_core::parse(plan).map_err(|error| error.to_string()))
        .collect()
}

/// The run that took the median time.
fn median(mut runs: Vec<Plan>) -> Plan {
    let time = |plan: &Plan| plan.summary.execution_time.unwrap_or(0.0);
    runs.sort_by(|a, b| time(a).total_cmp(&time(b)));
    let middle = runs.len() / 2;
    runs.swap_remove(middle)
}

/// `With $1 = 4242`, for a note about a trial.
fn values(trial: &Trial, parameters: &[params::Parameter]) -> String {
    let shown: Vec<String> = parameters
        .iter()
        .zip(&trial.values)
        .filter(|(parameter, _)| {
            trial
                .parameter
                .is_none_or(|number| number == parameter.number)
        })
        .map(|(parameter, value)| parameter.show(value.as_deref()))
        .collect();
    format!("With {}, the statement", shown.join(", "))
}

/// Reads `--bind N=VALUE`.
pub(crate) fn binding(text: &str) -> Result<(usize, String), String> {
    let (number, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text}: give a parameter's number and its value, as 2=pending"))?;
    let number = number.trim().trim_start_matches('$');
    match number.parse::<usize>() {
        Ok(number) if number >= 1 => Ok((number, value.to_owned())),
        _ => Err(format!(
            "{text}: {number} is not a parameter's number; $1 is 1"
        )),
    }
}
