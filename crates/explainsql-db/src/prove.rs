//! Checking a suggested index before creating it, two ways:
//!
//! - With HypoPG, a hypothetical index the planner can see but that holds no
//!   data: the plan with it is estimated, never run. It lives in the session
//!   only, so it is dropped right after, whatever happens.
//! - With `--allow-ddl`, the index is built for real inside a transaction
//!   that is rolled back, and the statement measured with EXPLAIN ANALYZE.
//!   Building blocks writes to the table, so `lock_timeout` gives up rather
//!   than wait behind other sessions, and the viewer asks first.

use tokio_postgres::Client;

use crate::exec::{self, Mode, Writes, options, statement};
use crate::{Error, Safety, describe};

/// How long building an index may wait for its lock.
const LOCK_TIMEOUT: &str = "2s";

/// The plans without and with the index, as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub before: String,
    pub after: String,
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
    let before = exec::explain(client, sql, Mode::Estimate, safety, server_version).await?;
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
        before,
        after: after.to_string(),
        measured: false,
    })
}

pub(crate) async fn measured(
    client: &Client,
    sql: &str,
    ddl: &str,
    safety: Safety,
    server_version: u32,
) -> Result<Proof, Error> {
    if !safety.allow_ddl {
        return Err(Error::Refused(
            "building the index needs --allow-ddl (or HypoPG, which needs nothing built)"
                .to_owned(),
        ));
    }
    let sql = statement(sql)?;
    let ddl = plain(ddl)?;
    let writes = exec::writes(client, sql, safety, server_version).await?;
    if writes != Writes::No && !safety.allow_dml {
        return Err(Error::NeedsAllowDml(
            "modifies data or locks rows".to_owned(),
        ));
    }
    let before = exec::explain(client, sql, Mode::Analyze, safety, server_version).await?;
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
        client.query_one(explain.as_str(), &[]).await
    }
    .await;
    // Always, whatever happened above: the index goes away with it.
    let rollback = client.batch_execute("ROLLBACK").await;
    let row = result.map_err(|error| {
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
    let after: serde_json::Value = row.try_get(0).map_err(server)?;
    Ok(Proof {
        before,
        after: after.to_string(),
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
