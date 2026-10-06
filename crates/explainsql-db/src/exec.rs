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

use std::time::Duration;

use explainsql_core::scenario::Setting;
use tokio_postgres::{Client, SimpleQueryMessage};

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
    let sql = statement(sql)?;
    check(settings)?;
    // Planned under the settings: whether the statement writes does not
    // depend on them.
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
    if mode == Mode::Estimate {
        return Ok(estimated);
    }
    let writes = allowed_writes(&estimated, safety)?;
    run(
        client,
        &format!("EXPLAIN ({}) {sql}", options(Mode::Analyze, server_version)),
        writes == Writes::No,
        safety.timeout,
        settings,
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
/// its own transaction that is rolled back.
pub(crate) async fn measure(
    client: &Client,
    sql: &str,
    settings: &[Setting],
    runs: usize,
    safety: Safety,
    server_version: u32,
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
    let explain = format!("EXPLAIN ({}) {sql}", options(Mode::Analyze, server_version));
    let mut plans = Vec::with_capacity(runs.max(1));
    for warm_up in std::iter::once(true).chain(std::iter::repeat_n(false, runs.max(1))) {
        let plan = run(
            client,
            &explain,
            writes == Writes::No,
            safety.timeout,
            settings,
        )
        .await?;
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

/// Runs one EXPLAIN inside a transaction that is rolled back, and returns
/// its JSON.
async fn run(
    client: &Client,
    explain: &str,
    read_only: bool,
    timeout: Duration,
    settings: &[Setting],
) -> Result<String, Error> {
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
        client.query(explain, &[]).await
    }
    .await;
    // Always, whatever happened above.
    let rollback = client.batch_execute("ROLLBACK").await;
    let rows = result.map_err(server)?;
    rollback.map_err(server)?;
    let row = rows
        .first()
        .ok_or_else(|| Error::Server("EXPLAIN returned no plan".to_owned()))?;
    let plan: serde_json::Value = row.try_get(0).map_err(server)?;
    Ok(plan.to_string())
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
            options(Mode::Analyze, 160_000),
            "ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON"
        );
    }
}
