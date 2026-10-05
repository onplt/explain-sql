//! Running `EXPLAIN` safely. Every run happens inside a transaction that
//! is rolled back, whatever happens, with a `statement_timeout`; there is no
//! code path that commits. The estimated plan comes first and tells whether
//! the statement modifies data or locks rows: those run only with
//! `--allow-dml`, and everything else runs in a `READ ONLY` transaction, so
//! that even a function that writes fails. Statements are sent with the
//! extended query protocol, which refuses several statements in one string.

use std::time::Duration;

use tokio_postgres::Client;

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
    safety: Safety,
    server_version: u32,
) -> Result<String, Error> {
    let sql = statement(sql)?;
    let estimated = run(
        client,
        &format!(
            "EXPLAIN ({}) {sql}",
            options(Mode::Estimate, server_version)
        ),
        true,
        safety.timeout,
    )
    .await?;
    if mode == Mode::Estimate {
        return Ok(estimated);
    }
    let writes = writes_of(&estimated);
    if let Some(reason) = writes.reason() {
        if !safety.allow_dml {
            return Err(Error::NeedsAllowDml(reason.to_owned()));
        }
    }
    run(
        client,
        &format!("EXPLAIN ({}) {sql}", options(Mode::Analyze, server_version)),
        writes == Writes::No,
        safety.timeout,
    )
    .await
}

/// Runs one EXPLAIN inside a transaction that is rolled back, and returns
/// its JSON.
async fn run(
    client: &Client,
    explain: &str,
    read_only: bool,
    timeout: Duration,
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
