//! Running `EXPLAIN` safely. Every run happens inside a transaction that
//! is rolled back, whatever happens, with a `statement_timeout`; there is no
//! code path that commits. The estimated plan comes first and tells whether
//! the statement modifies data or locks rows: those run only with
//! `--allow-dml`, and everything else runs in a `READ ONLY` transaction, so
//! that even a function that writes fails. Statements are sent with the
//! extended query protocol, which refuses several statements in one string.
//!
//! A statement can be planned under planner settings (`enable_seqscan =
//! off`, `work_mem = 64MB`): only those [`Setting::check`] accepts, set with
//! `set_config(…, true)` inside the transaction, so they end with it.
//!
//! Between the `EXPLAIN` and the rollback, the transaction still holds the
//! locks the statement took and counts what it wrote: a run can read them
//! there ([`Observe`]), and a second connection can watch what the
//! statement waits on as it runs. A statement that writes reports the WAL
//! it wrote too.

use std::time::Duration;

use explainsql_core::locks::{Capture, Stage, Waits};
use explainsql_core::scenario::Setting;
use explainsql_core::writes::WriteCapture;
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::locks::{self, Watch};
use crate::writes;
use crate::{Error, Safety, describe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `EXPLAIN`: the planner's estimates; the statement does not run.
    Estimate,
    /// `EXPLAIN ANALYZE`: the statement runs and is rolled back.
    Analyze,
}

/// Whether a statement changes anything when it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    No,
    /// `INSERT`, `UPDATE`, `DELETE`, `MERGE`, also inside a `WITH`.
    ModifiesData,
    /// `SELECT … FOR UPDATE` and the like.
    LocksRows,
}

impl Writes {
    fn reason(self) -> Option<&'static str> {
        match self {
            Writes::No => None,
            Writes::ModifiesData => Some("modifies data"),
            Writes::LocksRows => Some("locks rows (FOR UPDATE or FOR SHARE)"),
        }
    }
}

/// The statements EXPLAIN accepts that explainsql runs.
const EXPLAINABLE: [&str; 8] = [
    "SELECT", "WITH", "VALUES", "TABLE", "INSERT", "UPDATE", "DELETE", "MERGE",
];

/// The statement without surrounding blanks and trailing semicolons, if
/// explainsql runs statements of its kind.
pub(crate) fn statement(sql: &str) -> Result<&str, Error> {
    let sql = sql
        .trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    let keyword: String = skip_comments(sql)
        .trim_start_matches('(')
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    if keyword.is_empty() {
        return Err(Error::Refused(
            "there is no statement to explain".to_owned(),
        ));
    }
    if keyword == "EXPLAIN" {
        return Err(Error::Refused(
            "give the statement without EXPLAIN: explainsql adds the options it needs".to_owned(),
        ));
    }
    if !EXPLAINABLE.contains(&keyword.as_str()) {
        return Err(Error::Refused(format!(
            "explainsql explains SELECT, INSERT, UPDATE, DELETE and MERGE statements; this one starts with {keyword}"
        )));
    }
    Ok(sql)
}

/// The text after leading comments.
fn skip_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            sql = rest.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(rest) = sql.strip_prefix("/*") {
            sql = rest.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            return sql;
        }
    }
}

/// The options of `EXPLAIN ANALYZE` for a statement: for one that writes,
/// with the WAL it wrote, from PostgreSQL 13.
pub(crate) fn analyze_options(server_version: u32, writes: Writes) -> String {
    let options = options(Mode::Analyze, server_version);
    if writes != Writes::No && server_version >= 130_000 {
        options.replacen("BUFFERS", "BUFFERS, WAL", 1)
    } else {
        options
    }
}

pub(crate) fn options(mode: Mode, server_version: u32) -> String {
    let settings = if server_version >= 120_000 {
        ", SETTINGS"
    } else {
        ""
    };
    match mode {
        Mode::Estimate => format!("VERBOSE{settings}, FORMAT JSON"),
        Mode::Analyze => format!("ANALYZE, BUFFERS, VERBOSE{settings}, FORMAT JSON"),
    }
}

/// Refuses settings explainsql does not change, before anything runs.
pub(crate) fn check(settings: &[Setting]) -> Result<(), Error> {
    for setting in settings {
        setting.check().map_err(Error::Refused)?;
    }
    Ok(())
}

/// What a statement writes, from its estimated plan.
pub(crate) async fn writes(
    client: &Client,
    sql: &str,
    safety: Safety,
    server_version: u32,
) -> Result<Writes, Error> {
    let sql = statement(sql)?;
    let estimated = run(
        client,
        &format!(
            "EXPLAIN ({}) {sql}",
            options(Mode::Estimate, server_version)
        ),
        true,
        safety.timeout,
        &[],
    )
    .await?;
    Ok(writes_of(&estimated))
}

fn writes_of(plan: &str) -> Writes {
    let Ok(plan) = explainsql_core::parse(plan) else {
        // Unreadable: assume the worst.
        return Writes::ModifiesData;
    };
    if plan
        .nodes
        .iter()
        .any(|node| node.node_type == "ModifyTable")
    {
        Writes::ModifiesData
    } else if plan.nodes.iter().any(|node| node.node_type == "LockRows") {
        Writes::LocksRows
    } else {
        Writes::No
    }
}

pub(crate) async fn explain(
    client: &Client,
    sql: &str,
    mode: Mode,
    settings: &[Setting],
    safety: Safety,
    server_version: u32,
) -> Result<String, Error> {
    let observe = Observe::nothing(server_version);
    explain_observed(client, sql, mode, settings, safety, observe)
        .await
        .map(|(plan, _)| plan)
}

/// The plan of a statement, and what was observed of its last run: the
/// estimated one, or the one that ran with `EXPLAIN ANALYZE`.
pub(crate) async fn explain_observed(
    client: &Client,
    sql: &str,
    mode: Mode,
    settings: &[Setting],
    safety: Safety,
    observe: Observe<'_>,
) -> Result<(String, Observed), Error> {
    let sql = statement(sql)?;
    check(settings)?;
    let server_version = observe.server_version;
    let estimate = format!(
        "EXPLAIN ({}) {sql}",
        options(Mode::Estimate, server_version)
    );
    // Planned under the settings: whether the statement writes does not
    // depend on them.
    if mode == Mode::Estimate {
        // Nothing is written.
        let observe = Observe {
            writes: false,
            ..observe
        };
        return run_observed(client, &estimate, true, safety.timeout, settings, observe).await;
    }
    let estimated = run(client, &estimate, true, safety.timeout, settings).await?;
    let writes = allowed_writes(&estimated, safety)?;
    let observe = Observe {
        writes: observe.writes && writes != Writes::No,
        ..observe
    };
    run_observed(
        client,
        &format!(
            "EXPLAIN ({}) {sql}",
            analyze_options(server_version, writes)
        ),
        writes == Writes::No,
        safety.timeout,
        settings,
        observe,
    )
    .await
}

/// The generic plan of a statement with parameters (`$1`, `$2`, …) as the
/// planner makes it for any value, without values and without running it:
/// `EXPLAIN (GENERIC_PLAN)`, PostgreSQL 16 or later.
///
/// Sent with the extended query protocol, the statement's `$1` would be a
/// parameter of the EXPLAIN itself, to be bound to a value that the planner
/// then plans for. So the EXPLAIN goes with the simple query protocol,
/// which leaves the parameters to GENERIC_PLAN. That protocol also runs
/// several statements in one string: the statement is first parsed alone
/// with the extended protocol, which refuses more than one, and nothing
/// runs until it has been.
pub(crate) async fn generic(
    client: &Client,
    sql: &str,
    safety: Safety,
    server_version: u32,
) -> Result<String, Error> {
    let sql = statement(sql)?;
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}",
                safety.timeout.as_millis().max(1)
            ))
            .await?;
        // One statement, or an error: parsed, not run.
        drop(client.prepare(sql).await?);
        client
            .simple_query(&format!(
                "EXPLAIN (GENERIC_PLAN, {}) {sql}",
                options(Mode::Estimate, server_version)
            ))
            .await
    }
    .await;
    // Always, whatever happened above.
    let rollback = client.batch_execute("ROLLBACK").await;
    let messages = result.map_err(server)?;
    rollback.map_err(server)?;
    let mut rows = messages.iter().filter_map(|message| match message {
        SimpleQueryMessage::Row(row) => Some(row),
        _ => None,
    });
    let text = rows
        .next()
        .and_then(|row| row.get(0))
        .ok_or_else(|| Error::Server("EXPLAIN returned no plan".to_owned()))?;
    let plan: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        Error::Server(format!("EXPLAIN returned a plan that is not JSON: {error}"))
    })?;
    Ok(plan.to_string())
}

/// Measures a statement `runs` times with EXPLAIN ANALYZE, after one more
/// run that only warms the cache, so that each measured run finds what the
/// statement reads already cached, as the others do. Each run happens in
/// its own transaction that is rolled back. Watched, a measured run that
/// waited for another session's lock is run again, with a note.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn measure(
    client: &Client,
    sql: &str,
    settings: &[Setting],
    runs: usize,
    safety: Safety,
    server_version: u32,
    watch: Option<&Watch>,
    notes: &mut Vec<String>,
) -> Result<Vec<String>, Error> {
    let sql = statement(sql)?;
    check(settings)?;
    let estimated = run(
        client,
        &format!(
            "EXPLAIN ({}) {sql}",
            options(Mode::Estimate, server_version)
        ),
        true,
        safety.timeout,
        settings,
    )
    .await?;
    let writes = allowed_writes(&estimated, safety)?;
    let explain = format!(
        "EXPLAIN ({}) {sql}",
        analyze_options(server_version, writes)
    );
    let mut plans = Vec::with_capacity(runs.max(1));
    for warm_up in std::iter::once(true).chain(std::iter::repeat_n(false, runs.max(1))) {
        let observe = Observe {
            watch: watch.filter(|_| !warm_up),
            ..Observe::nothing(server_version)
        };
        let mut tries = 0;
        let plan = loop {
            let (plan, observed) = run_observed(
                client,
                &explain,
                writes == Writes::No,
                safety.timeout,
                settings,
                observe,
            )
            .await?;
            if !locks::again(observed.waits.as_ref(), &mut tries, notes) {
                break plan;
            }
        };
        if !warm_up {
            plans.push(plan);
        }
    }
    Ok(plans)
}

/// What the estimated plan says the statement writes, if `--allow-dml`
/// lets it run.
pub(crate) fn allowed_writes(estimated: &str, safety: Safety) -> Result<Writes, Error> {
    let writes = writes_of(estimated);
    match writes.reason() {
        Some(reason) if !safety.allow_dml => Err(Error::NeedsAllowDml(reason.to_owned())),
        _ => Ok(writes),
    }
}

/// Sets planner settings until the end of the transaction. The names and
/// values are bound as parameters, never spliced into the SQL.
pub(crate) async fn apply(
    client: &Client,
    settings: &[Setting],
) -> Result<(), tokio_postgres::Error> {
    for setting in settings {
        client
            .query(
                "SELECT set_config($1, $2, true)",
                &[&setting.name, &setting.value],
            )
            .await?;
    }
    Ok(())
}

/// What to observe of a run besides its plan.
#[derive(Clone, Copy)]
pub(crate) struct Observe<'a> {
    /// Read the locks the statement took, before the rollback.
    pub locks: Option<Stage>,
    /// Sample what the statement waits on as it runs.
    pub watch: Option<&'a Watch>,
    /// Read what the statement wrote, table by table.
    pub writes: bool,
    pub server_version: u32,
}

impl Observe<'_> {
    pub(crate) fn nothing(server_version: u32) -> Self {
        Observe {
            locks: None,
            watch: None,
            writes: false,
            server_version,
        }
    }
}

/// What was observed of a run. `None` also for what could not be read.
#[derive(Debug, Default)]
pub(crate) struct Observed {
    pub locks: Option<Capture>,
    pub waits: Option<Waits>,
    pub writes: Option<WriteCapture>,
}

/// Runs one EXPLAIN inside a transaction that is rolled back, and returns
/// its JSON.
async fn run(
    client: &Client,
    explain: &str,
    read_only: bool,
    timeout: Duration,
    settings: &[Setting],
) -> Result<String, Error> {
    // The server version matters only to read locks.
    run_observed(
        client,
        explain,
        read_only,
        timeout,
        settings,
        Observe::nothing(0),
    )
    .await
    .map(|(plan, _)| plan)
}

/// Runs one EXPLAIN inside a transaction that is rolled back, and returns
/// its JSON and what was observed: the locks are read after the EXPLAIN,
/// before the rollback releases them.
pub(crate) async fn run_observed(
    client: &Client,
    explain: &str,
    read_only: bool,
    timeout: Duration,
    settings: &[Setting],
    observe: Observe<'_>,
) -> Result<(String, Observed), Error> {
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    client
        .batch_execute(if read_only {
            "BEGIN READ ONLY"
        } else {
            "BEGIN"
        })
        .await
        .map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}",
                timeout.as_millis().max(1)
            ))
            .await?;
        apply(client, settings).await?;
        in_transaction(client, observe, client.query(explain, &[])).await
    }
    .await;
    // Always, whatever happened above.
    let rollback = client.batch_execute("ROLLBACK").await;
    let (rows, observed) = result.map_err(server)?;
    rollback.map_err(server)?;
    let row = rows
        .first()
        .ok_or_else(|| Error::Server("EXPLAIN returned no plan".to_owned()))?;
    let plan: serde_json::Value = row.try_get(0).map_err(server)?;
    Ok((plan.to_string(), observed))
}

/// Runs the EXPLAIN of an open transaction, watched if asked, and then
/// reads the locks and what it wrote if asked. What cannot be read after
/// the EXPLAIN is left out rather than failing the run.
pub(crate) async fn in_transaction<T>(
    client: &Client,
    observe: Observe<'_>,
    explain: impl Future<Output = Result<T, tokio_postgres::Error>>,
) -> Result<(T, Observed), tokio_postgres::Error> {
    let before = if observe.writes {
        Some(writes::before(client, observe.server_version).await?)
    } else {
        None
    };
    let sampler = match observe.watch {
        Some(watch) => {
            let pid: i32 = client
                .query_one("SELECT pg_catalog.pg_backend_pid()", &[])
                .await?
                .get(0);
            Some(watch.start(pid))
        }
        None => None,
    };
    let result = explain.await;
    let waits = match sampler {
        Some(sampler) => sampler.stop().await,
        None => None,
    };
    let rows = result?;
    let mut locks = match observe.locks {
        Some(stage) => locks::capture(client, stage, observe.server_version)
            .await
            .ok(),
        None => None,
    };
    if let Some(locks) = &mut locks {
        locks.waits.clone_from(&waits);
    }
    // After the locks: reading this takes locks of its own.
    let writes = match &before {
        Some(before) => writes::capture(client, before, observe.server_version)
            .await
            .ok(),
        None => None,
    };
    Ok((
        rows,
        Observed {
            locks,
            waits,
            writes,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_explainable_statements() {
        assert_eq!(statement("  SELECT 1;  \n").unwrap(), "SELECT 1");
        assert!(statement("-- note\n/* x */ (SELECT 1) UNION SELECT 2").is_ok());
        assert!(statement("with x as (select 1) select * from x").is_ok());
        assert!(matches!(
            statement("DROP TABLE orders"),
            Err(Error::Refused(_))
        ));
        assert!(
            matches!(statement("EXPLAIN SELECT 1"), Err(Error::Refused(m)) if m.contains("without EXPLAIN"))
        );
        assert!(matches!(statement(" ;"), Err(Error::Refused(_))));
        assert!(matches!(
            statement("CREATE TABLE t AS SELECT 1"),
            Err(Error::Refused(_))
        ));
    }

    #[test]
    fn asks_for_version_specific_options() {
        assert_eq!(options(Mode::Estimate, 110_000), "VERBOSE, FORMAT JSON");
        assert_eq!(
            analyze_options(160_000, Writes::ModifiesData),
            "ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS, FORMAT JSON"
        );
        assert_eq!(
            analyze_options(120_000, Writes::ModifiesData),
            "ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON"
        );
        assert_eq!(
            options(Mode::Analyze, 160_000),
            "ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON"
        );
    }
}
