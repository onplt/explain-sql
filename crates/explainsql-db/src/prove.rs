//! Checking a suggested index before creating it, two ways:
//!
//! - With HypoPG, a hypothetical index the planner can see but that holds no
//!   data: the plan with it is estimated, never run. It lives in the session
//!   only, so it is dropped right after, whatever happens.
//! - With `--allow-ddl`, the index is built for real inside a transaction
//!   that is rolled back, and the statement measured with EXPLAIN ANALYZE.
//!   Building blocks writes to the table, so `lock_timeout` gives up rather
//!   than wait behind other sessions, and the viewer asks first.
//!
//! Measured runs on both sides follow one run that only warms the cache. A
//! run before the index would otherwise often meet a colder cache than the
//! runs after it, which follow the build that read the whole table.

use tokio_postgres::Client;

use crate::exec::{self, Mode, options, statement};
use crate::locks::Watch;
use crate::{Error, Safety, describe};

/// How long building an index may wait for its lock.
const LOCK_TIMEOUT: &str = "2s";

/// The plans without and with the index, as JSON: one estimated plan on
/// each side, or every measured run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub before: Vec<String>,
    pub after: Vec<String>,
    /// Measured with EXPLAIN ANALYZE rather than estimated.
    pub measured: bool,
}

/// `CREATE INDEX` without `CONCURRENTLY`, which neither HypoPG nor a
/// transaction accepts.
fn plain(ddl: &str) -> Result<String, Error> {
    let ddl = ddl.trim().trim_end_matches(';');
    if !ddl.starts_with("CREATE INDEX ") || ddl.contains(';') {
        return Err(Error::Refused(format!(
            "not a CREATE INDEX statement: {ddl}"
        )));
    }
    Ok(ddl.replacen("CREATE INDEX CONCURRENTLY ", "CREATE INDEX ", 1))
}

pub(crate) async fn hypothetical(
    client: &Client,
    sql: &str,
    ddl: &str,
    safety: Safety,
    server_version: u32,
) -> Result<Proof, Error> {
    let sql = statement(sql)?;
    let ddl = plain(ddl)?;
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    let explain = format!(
        "EXPLAIN ({}) {sql}",
        options(Mode::Estimate, server_version)
    );
    let before = exec::explain(client, sql, Mode::Estimate, &[], safety, server_version).await?;
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
        client
            .query("SELECT indexrelid FROM hypopg_create_index($1)", &[&ddl])
            .await?;
        client.query_one(explain.as_str(), &[]).await
    }
    .await;
    // Always: hypothetical indexes outlive transactions.
    let reset = client.batch_execute("SELECT hypopg_reset()").await;
    let rollback = client.batch_execute("ROLLBACK").await;
    let row = result.map_err(server)?;
    reset.map_err(server)?;
    rollback.map_err(server)?;
    let after: serde_json::Value = row.try_get(0).map_err(server)?;
    Ok(Proof {
        before: vec![before],
        after: vec![after.to_string()],
        measured: false,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn measured(
    client: &Client,
    sql: &str,
    ddl: &str,
    runs: usize,
    safety: Safety,
    server_version: u32,
    watch: Option<&Watch>,
    notes: &mut Vec<String>,
) -> Result<Proof, Error> {
    if !safety.allow_ddl {
        return Err(Error::Refused(
            "building the index needs --allow-ddl (or HypoPG, which needs nothing built)"
                .to_owned(),
        ));
    }
    let sql = statement(sql)?;
    let ddl = plain(ddl)?;
    // Refuses a statement that writes without --allow-dml, before anything
    // is built.
    let before =
        exec::measure(client, sql, &[], runs, safety, server_version, watch, notes).await?;
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    let explain = format!("EXPLAIN ({}) {sql}", options(Mode::Analyze, server_version));
    client.batch_execute("BEGIN").await.map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}; SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'",
                safety.timeout.as_millis().max(1)
            ))
            .await?;
        client.batch_execute(&ddl).await?;
        // The first run warms the cache with the pages the statement reads
        // through the index, as the first run before did without it.
        let mut rows = Vec::with_capacity(runs.max(1));
        for warm_up in std::iter::once(true).chain(std::iter::repeat_n(false, runs.max(1))) {
            let row = client.query_one(explain.as_str(), &[]).await?;
            if !warm_up {
                rows.push(row);
            }
        }
        Ok::<_, tokio_postgres::Error>(rows)
    }
    .await;
    // Always, whatever happened above: the index goes away with it.
    let rollback = client.batch_execute("ROLLBACK").await;
    let rows = result.map_err(|error: tokio_postgres::Error| {
        let text = describe(&error);
        if text.contains("55P03") {
            Error::Server(format!(
                "the table is busy: building the index waited more than {LOCK_TIMEOUT} for its lock ({text})"
            ))
        } else {
            Error::Server(text)
        }
    })?;
    rollback.map_err(server)?;
    let after = rows
        .iter()
        .map(|row| {
            row.try_get::<_, serde_json::Value>(0)
                .map(|plan| plan.to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(server)?;
    Ok(Proof {
        before,
        after,
        measured: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_one_create_index() {
        assert_eq!(
            plain("CREATE INDEX CONCURRENTLY ON public.orders (customer_id);").unwrap(),
            "CREATE INDEX ON public.orders (customer_id)"
        );
        assert!(plain("DROP TABLE orders").is_err());
        assert!(plain("CREATE INDEX ON t (a); DROP TABLE t").is_err());
    }
}
