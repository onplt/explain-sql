//! Reads the costliest statements from pg_stat_statements, with the SQL its
//! version calls for, and plans one of them with `GENERIC_PLAN`.

use explainsql_core::top::{self, Entry};
use tokio_postgres::Client;

use crate::{Error, Safety, describe};

/// The schema pg_stat_statements is installed in, in this database.
const EXTENSION: &str = "
SELECT n.nspname::text
FROM pg_extension e
JOIN pg_namespace n ON n.oid = e.extnamespace
WHERE e.extname = 'pg_stat_statements'";

/// The SQL that reads the `limit` statements of the current database with
/// the most execution time, with the time of all of them, for a server
/// version: `total_time` became `total_exec_time` in 13, and `toplevel`
/// (14) leaves out statements run inside functions, already counted in the
/// statement that called them.
pub(crate) fn statements_sql(server_version: u32, schema: &str) -> String {
    let (total, mean) = if server_version >= 130_000 {
        ("total_exec_time", "mean_exec_time")
    } else {
        ("total_time", "mean_time")
    };
    let toplevel = if server_version >= 140_000 {
        " AND toplevel"
    } else {
        ""
    };
    format!(
        "SELECT queryid, query, calls, {total}::float8, {mean}::float8, rows, \
         shared_blks_hit, shared_blks_read, temp_blks_written, \
         (sum({total}) OVER ())::float8 \
         FROM {}.pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()){toplevel} \
         ORDER BY {total} DESC \
         LIMIT $1",
        quote_ident(schema)
    )
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The `limit` statements with the most execution time, in a READ ONLY
/// transaction that is rolled back.
pub(crate) async fn statements(
    client: &Client,
    limit: usize,
    safety: Safety,
    server_version: u32,
) -> Result<Vec<Entry>, Error> {
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
            .await
            .map_err(server)?;
        let Some(row) = client.query_opt(EXTENSION, &[]).await.map_err(server)? else {
            return Err(Error::Refused(
                "pg_stat_statements is not installed in this database: run CREATE EXTENSION pg_stat_statements, with pg_stat_statements in shared_preload_libraries".to_owned(),
            ));
        };
        let schema: String = row.get(0);
        let size: Option<String> = client
            .query_one("SELECT current_setting('track_activity_query_size')", &[])
            .await
            .map_err(server)?
            .get(0);
        let size = size.and_then(|size| bytes(&size));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(&statements_sql(server_version, &schema), &[&limit])
            .await
            .map_err(|error| match error.as_db_error() {
                // 55000: the library was not loaded at server start.
                Some(db) if db.code().code() == "55000" => Error::Refused(
                    "pg_stat_statements is installed but not loaded: add it to shared_preload_libraries and restart the server".to_owned(),
                ),
                _ => server(error),
            })?;
        Ok(rows
            .iter()
            .map(|row| {
                // Null when pg_stat_statements could not find the text;
                // another user's is HIDDEN.
                let query: Option<String> = row.get(1);
                let query = query.unwrap_or_else(|| top::LOST.to_owned());
                let total_ms: f64 = row.get(3);
                let all_ms: f64 = row.get(9);
                Entry {
                    queryid: row.get(0),
                    unplannable: top::unplannable(&query, size),
                    query,
                    calls: row.get(2),
                    total_ms,
                    share: if all_ms > 0.0 { total_ms / all_ms } else { 0.0 },
                    mean_ms: row.get(4),
                    rows: row.get(5),
                    shared_hit: row.get(6),
                    shared_read: row.get(7),
                    temp_written: row.get(8),
                }
            })
            .collect())
    }
    .await;
    // Always, whatever happened above.
    let rollback = client.batch_execute("ROLLBACK").await;
    let entries = result?;
    rollback.map_err(server)?;
    Ok(entries)
}

/// `track_activity_query_size` in bytes: `1024`, `1kB`, `4MB`.
fn bytes(setting: &str) -> Option<usize> {
    let digits: String = setting.chars().take_while(char::is_ascii_digit).collect();
    let number: usize = digits.parse().ok()?;
    let unit = match setting[digits.len()..].trim() {
        "" | "B" => 1,
        "kB" => 1024,
        "MB" => 1024 * 1024,
        _ => return None,
    };
    number.checked_mul(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_each_version_for_its_columns() {
        let sql = statements_sql(120_000, "public");
        assert!(
            sql.contains("total_time::float8, mean_time::float8"),
            "{sql}"
        );
        assert!(sql.contains("ORDER BY total_time DESC"), "{sql}");
        assert!(sql.contains("(sum(total_time) OVER ())"), "{sql}");
        assert!(!sql.contains("toplevel"), "{sql}");
        let sql = statements_sql(130_000, "public");
        assert!(sql.contains("total_exec_time::float8, mean_exec_time::float8"));
        assert!(!sql.contains("toplevel"), "{sql}");
        for version in [140_000, 150_000, 160_004, 170_000, 180_000] {
            let sql = statements_sql(version, "public");
            assert!(sql.contains("total_exec_time"), "{version}: {sql}");
            assert!(sql.contains("AND toplevel"), "{version}: {sql}");
            assert!(sql.contains("temp_blks_written"), "{version}: {sql}");
        }
        assert!(
            statements_sql(160_000, "my \"stats\"")
                .contains("FROM \"my \"\"stats\"\"\".pg_stat_statements")
        );
    }

    #[test]
    fn reads_the_query_size() {
        assert_eq!(bytes("1024"), Some(1024));
        assert_eq!(bytes("1kB"), Some(1024));
        assert_eq!(bytes("4MB"), Some(4 * 1024 * 1024));
        assert_eq!(bytes("lots"), None);
    }
}
