//! Statements with parameters, run as an application runs them: prepared,
//! then executed with values. `plan_cache_mode` chooses the plan: the
//! generic one, for any value, or a custom one, for the values given.
//!
//! Each run happens, like every other, inside a transaction that is rolled
//! back, `READ ONLY` unless the statement writes and `--allow-dml` lets it,
//! with a `statement_timeout`. `PREPARE` is not undone by a rollback: the
//! statement is deallocated after it, whatever happened. The statement is
//! sent with the extended query protocol, which refuses several statements
//! in one string, and the values as dollar-quoted literals whose tag they
//! do not contain.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use explainsql_core::locks::{Capture, Stage};
use explainsql_core::params::ColumnStats;
use explainsql_core::scenario::Setting;
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

use crate::exec::{self, Mode, Observe, Observed, Writes};
use crate::locks::{self, Watch};
use crate::{Error, Safety, describe};

/// Which plan a prepared statement gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// The generic plan, for any value (`force_generic_plan`).
    Generic,
    /// A plan for the values given (`force_custom_plan`).
    Custom,
}

impl Cache {
    fn setting(self) -> Setting {
        Setting::new(
            "plan_cache_mode",
            match self {
                Cache::Generic => "force_generic_plan",
                Cache::Custom => "force_custom_plan",
            },
        )
    }
}

/// Tells the statements explainsql prepares apart within a session.
static NEXT: AtomicU64 = AtomicU64::new(1);

fn next_name() -> String {
    format!("explainsql_{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The plan of a statement with parameters, run with `values` under
/// planner settings: estimated, or measured with EXPLAIN ANALYZE when the
/// statement's estimated plan shows it may run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn explain(
    client: &Client,
    sql: &str,
    cache: Cache,
    values: &[Option<String>],
    settings: &[Setting],
    mode: Mode,
    safety: Safety,
    server_version: u32,
) -> Result<String, Error> {
    let sql = exec::statement(sql)?;
    exec::check(settings)?;
    let mut settings = settings.to_vec();
    settings.push(cache.setting());
    let estimate = exec::options(Mode::Estimate, server_version);
    let estimated = run(
        client,
        sql,
        &estimate,
        true,
        safety.timeout,
        &settings,
        values,
    )
    .await?;
    if mode == Mode::Estimate {
        return Ok(estimated);
    }
    let writes = exec::allowed_writes(&estimated, safety)?;
    let analyze = exec::options(Mode::Analyze, server_version);
    run(
        client,
        sql,
        &analyze,
        writes == Writes::No,
        safety.timeout,
        &settings,
        values,
    )
    .await
}

/// `runs` measured plans of a statement with parameters, after a run that
/// only warms the cache. Each run is rolled back. Watched, a measured run
/// that waited for another session's lock is run again, with a note.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn measure(
    client: &Client,
    sql: &str,
    cache: Cache,
    values: &[Option<String>],
    runs: usize,
    safety: Safety,
    server_version: u32,
    watch: Option<&Watch>,
    notes: &mut Vec<String>,
) -> Result<Vec<String>, Error> {
    let sql = exec::statement(sql)?;
    let settings = [cache.setting()];
    let estimate = exec::options(Mode::Estimate, server_version);
    let estimated = run(
        client,
        sql,
        &estimate,
        true,
        safety.timeout,
        &settings,
        values,
    )
    .await?;
    let writes = exec::allowed_writes(&estimated, safety)?;
    let analyze = exec::options(Mode::Analyze, server_version);
    let mut plans = Vec::with_capacity(runs.max(1));
    for warm_up in std::iter::once(true).chain(std::iter::repeat_n(false, runs.max(1))) {
        let observe = Observe {
            watch: watch.filter(|_| !warm_up),
            ..Observe::nothing(server_version)
        };
        let mut tries = 0;
        let plan = loop {
            let name = next_name();
            let result = run_named(
                client,
                &name,
                Some(sql),
                &analyze,
                writes == Writes::No,
                safety.timeout,
                &settings,
                values,
                observe,
            )
            .await;
            let deallocate = deallocate(client, &name).await;
            let (plan, observed) = result?;
            deallocate.map_err(|error| Error::Server(describe(&error)))?;
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

/// The plan of one execution of a statement with parameters, and the locks
/// that execution took. The statement is prepared and its plan made in one
/// transaction, then executed in a second, whose locks are read: a cached
/// generic plan is not planned again, so they are those every later
/// execution of it takes. Both transactions are rolled back, and the
/// statement deallocated after them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn locks(
    client: &Client,
    sql: &str,
    cache: Cache,
    values: &[Option<String>],
    mode: Mode,
    safety: Safety,
    server_version: u32,
    watch: Option<&Watch>,
) -> Result<(String, Capture), Error> {
    let sql = exec::statement(sql)?;
    let settings = [cache.setting()];
    let name = next_name();
    let estimate = exec::options(Mode::Estimate, server_version);
    let observe = Observe {
        locks: Some(match cache {
            Cache::Generic => Stage::Generic,
            Cache::Custom => Stage::Custom,
        }),
        watch,
        server_version,
    };
    let result = async {
        let (estimated, _) = run_named(
            client,
            &name,
            Some(sql),
            &estimate,
            true,
            safety.timeout,
            &settings,
            values,
            Observe::nothing(server_version),
        )
        .await?;
        let read_only = match mode {
            Mode::Estimate => true,
            Mode::Analyze => exec::allowed_writes(&estimated, safety)? == Writes::No,
        };
        run_named(
            client,
            &name,
            None,
            &exec::options(mode, server_version),
            read_only,
            safety.timeout,
            &settings,
            values,
            observe,
        )
        .await
    }
    .await;
    // Always, whatever happened above.
    let deallocate = deallocate(client, &name).await;
    let (plan, observed) = result?;
    deallocate.map_err(|error| Error::Server(describe(&error)))?;
    let capture = observed.locks.ok_or_else(|| {
        Error::Server("the locks the statement took could not be read".to_owned())
    })?;
    Ok((plan, capture))
}

/// The types PostgreSQL infers for a statement's parameters.
pub(crate) async fn parameter_types(
    client: &Client,
    sql: &str,
    timeout: Duration,
) -> Result<Vec<String>, Error> {
    let sql = exec::statement(sql)?;
    let name = next_name();
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}",
                timeout.as_millis().max(1)
            ))
            .await?;
        client
            .execute(format!("PREPARE {name} AS {sql}").as_str(), &[])
            .await?;
        let row = client
            .query_one(
                "SELECT ARRAY(SELECT format_type(t, NULL) FROM unnest(parameter_types) AS t)
                 FROM pg_prepared_statements WHERE name = $1",
                &[&name],
            )
            .await?;
        row.try_get::<_, Vec<String>>(0)
    }
    .await;
    let rollback = client.batch_execute("ROLLBACK").await;
    let deallocate = deallocate(client, &name).await;
    let types = result.map_err(server)?;
    rollback.map_err(server)?;
    deallocate.map_err(server)?;
    Ok(types)
}

/// One `EXPLAIN EXECUTE` of the statement under settings, prepared inside
/// a transaction that is rolled back, and deallocated after it. Each run
/// prepares the statement again, so that no plan is cached from another.
async fn run(
    client: &Client,
    sql: &str,
    options: &str,
    read_only: bool,
    timeout: Duration,
    settings: &[Setting],
    values: &[Option<String>],
) -> Result<String, Error> {
    let name = next_name();
    let result = run_named(
        client,
        &name,
        Some(sql),
        options,
        read_only,
        timeout,
        settings,
        values,
        Observe::nothing(0),
    )
    .await;
    // Always, whatever happened; the prepared statement outlives the
    // transaction.
    let deallocate = deallocate(client, &name).await;
    let (plan, _) = result?;
    deallocate.map_err(|error| Error::Server(describe(&error)))?;
    Ok(plan)
}

/// One `EXPLAIN EXECUTE` of the prepared statement `name` under settings,
/// inside a transaction that is rolled back; `sql` prepares it first. The
/// statement outlives the transaction: the caller deallocates it.
#[allow(clippy::too_many_arguments)]
async fn run_named(
    client: &Client,
    name: &str,
    sql: Option<&str>,
    options: &str,
    read_only: bool,
    timeout: Duration,
    settings: &[Setting],
    values: &[Option<String>],
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
        exec::apply(client, settings).await?;
        if let Some(sql) = sql {
            client
                .execute(format!("PREPARE {name} AS {sql}").as_str(), &[])
                .await?;
        }
        let explain = format!("EXPLAIN ({options}) EXECUTE {name}{}", arguments(values));
        exec::in_transaction(client, observe, client.query(explain.as_str(), &[])).await
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

/// Deallocates a prepared statement; one that was never prepared, because
/// `PREPARE` failed, is not an error.
async fn deallocate(client: &Client, name: &str) -> Result<(), tokio_postgres::Error> {
    match client.batch_execute(&format!("DEALLOCATE {name}")).await {
        Err(error) if error.code() == Some(&SqlState::INVALID_SQL_STATEMENT_NAME) => Ok(()),
        other => other,
    }
}

/// `('pending', NULL)`: the values as literals of unknown type, which
/// PostgreSQL converts to the parameters' types.
fn arguments(values: &[Option<String>]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let literals: Vec<String> = values
        .iter()
        .map(|value| match value {
            Some(value) => dollar_quoted(value),
            None => "NULL".to_owned(),
        })
        .collect();
    format!("({})", literals.join(", "))
}

/// A value in dollar quotes whose tag it does not contain, even across its
/// end: safe whatever the value and whatever `standard_conforming_strings`.
fn dollar_quoted(value: &str) -> String {
    let mut number = 0;
    loop {
        let tag = if number == 0 {
            "$v$".to_owned()
        } else {
            format!("$v{number}$")
        };
        let quoted = format!("{value}{tag}");
        if quoted.find(&tag) == Some(value.len()) {
            return format!("{tag}{quoted}");
        }
        number += 1;
    }
}

/// The statistics of a column: of the partitioned table, when the table is
/// a partition and its partitioned table has them, else of the table.
const COLUMN_STATS: &str = "
WITH target AS (
    SELECT c.oid
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relname = $2 AND ($1::text IS NULL OR n.nspname = $1)
    ORDER BY n.nspname = current_schema() DESC
    LIMIT 1
), candidates AS (
    SELECT pg_partition_root(oid) AS relid, 1 AS rank FROM target
    UNION ALL
    SELECT oid, 2 FROM target
)
SELECT c.relname::text, s.null_frac::float8, s.n_distinct::float8,
       coalesce(s.most_common_vals::text::text[], '{}'),
       coalesce(s.most_common_freqs::float8[], '{}'),
       coalesce(s.histogram_bounds::text::text[], '{}')
FROM candidates
JOIN pg_class c ON c.oid = candidates.relid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_stats s ON s.schemaname = n.nspname AND s.tablename = c.relname AND s.attname = $3
ORDER BY candidates.rank, s.inherited DESC
LIMIT 1";

/// What `pg_stats` says about a column, if it has statistics that read as
/// a list of values: those of the partitioned table for a partition.
pub(crate) async fn column_stats(
    client: &Client,
    schema: Option<&str>,
    table: &str,
    column: &str,
    timeout: Duration,
) -> Result<Option<ColumnStats>, Error> {
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    client
        .batch_execute("BEGIN READ ONLY")
        .await
        .map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}",
                timeout.as_millis().max(1)
            ))
            .await?;
        client
            .query_opt(COLUMN_STATS, &[&schema, &table, &column])
            .await
    }
    .await;
    let rollback = client.batch_execute("ROLLBACK").await;
    rollback.map_err(server)?;
    let row = match result {
        Ok(row) => row,
        // Values of an array type do not read as a list of text: no
        // values to try.
        Err(error) if error.code() == Some(&SqlState::INVALID_TEXT_REPRESENTATION) => {
            return Ok(None);
        }
        Err(error) => return Err(server(error)),
    };
    let Some(row) = row else {
        return Ok(None);
    };
    let read = || -> Result<ColumnStats, tokio_postgres::Error> {
        Ok(ColumnStats {
            table: row.try_get(0)?,
            null_frac: row.try_get(1)?,
            n_distinct: row.try_get(2)?,
            common_values: row.try_get(3)?,
            common_freqs: row.try_get(4)?,
            histogram: row.try_get(5)?,
        })
    };
    read().map(Some).map_err(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_values_safely() {
        assert_eq!(dollar_quoted("pending"), "$v$pending$v$");
        assert_eq!(dollar_quoted("it's"), "$v$it's$v$");
        // A value with the tag, or one that would end it early.
        assert_eq!(dollar_quoted("a$v$b"), "$v1$a$v$b$v1$");
        assert_eq!(dollar_quoted("x$v"), "$v1$x$v$v1$");
        assert_eq!(arguments(&[]), "");
        assert_eq!(arguments(&[Some("1".to_owned()), None]), "($v$1$v$, NULL)");
    }
}
