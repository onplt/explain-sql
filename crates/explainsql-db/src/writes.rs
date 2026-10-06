//! Reading what a statement wrote: the transaction's own counters of rows
//! inserted, updated, HOT-updated and deleted, by table, from
//! `pg_stat_xact_user_tables`, read before and after the statement inside
//! the transaction that is rolled back. Counters of earlier transactions
//! of the backend that it has not yet reported stay in that view, so the
//! statement's rows are the difference. The tables written are then
//! described from the catalog: their indexes, the columns each refers to,
//! and their fillfactor.
//!
//! The first read happens in a savepoint that is rolled back, so that the
//! locks it takes on the catalog are not counted among the statement's.
//!
//! With `--allow-ddl`, [`prove`] drops the indexes that keep updates from
//! being HOT inside a transaction that is rolled back, and runs the
//! statement again.

use std::collections::HashMap;

use explainsql_core::locks::QualifiedName;
use explainsql_core::writes::{IndexColumns, TableWrites, WriteCapture};
use tokio_postgres::Client;

use crate::exec::{self, Observe, Observed, Writes};
use crate::{Error, Safety, describe};

/// A table's counters in the transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    inserted: i64,
    updated: i64,
    deleted: i64,
    hot_updated: i64,
    newpage_updated: Option<i64>,
}

/// The counters of the tables the transaction wrote, by table.
pub(crate) type Before = HashMap<u32, Counts>;

fn counts_query(server_version: u32) -> String {
    let newpage = if server_version >= 160_000 {
        "n_tup_newpage_upd"
    } else {
        "NULL::int8"
    };
    format!(
        "SELECT relid, n_tup_ins, n_tup_upd, n_tup_del, n_tup_hot_upd, {newpage}
         FROM pg_catalog.pg_stat_xact_user_tables
         WHERE n_tup_ins > 0 OR n_tup_upd > 0 OR n_tup_del > 0"
    )
}

async fn counts(client: &Client, server_version: u32) -> Result<Before, tokio_postgres::Error> {
    Ok(client
        .query(&counts_query(server_version), &[])
        .await?
        .iter()
        .map(|row| {
            (
                row.get(0),
                Counts {
                    inserted: row.get(1),
                    updated: row.get(2),
                    deleted: row.get(3),
                    hot_updated: row.get(4),
                    newpage_updated: row.get(5),
                },
            )
        })
        .collect())
}

/// The counters before the statement, read in a savepoint that is rolled
/// back, which releases the locks the read took.
pub(crate) async fn before(
    client: &Client,
    server_version: u32,
) -> Result<Before, tokio_postgres::Error> {
    client.batch_execute("SAVEPOINT explainsql_writes").await?;
    let read = counts(client, server_version).await;
    client
        .batch_execute(
            "ROLLBACK TO SAVEPOINT explainsql_writes; RELEASE SAVEPOINT explainsql_writes",
        )
        .await?;
    read
}

/// The tables the statement wrote, and what it wrote to each.
pub(crate) async fn capture(
    client: &Client,
    before: &Before,
    server_version: u32,
) -> Result<WriteCapture, tokio_postgres::Error> {
    let after = counts(client, server_version).await?;
    let mut written: Vec<(u32, Counts)> = after
        .into_iter()
        .filter_map(|(relid, after)| {
            let before = before.get(&relid).copied().unwrap_or_default();
            let counts = Counts {
                inserted: after.inserted - before.inserted,
                updated: after.updated - before.updated,
                deleted: after.deleted - before.deleted,
                hot_updated: after.hot_updated - before.hot_updated,
                newpage_updated: after
                    .newpage_updated
                    .map(|newpage| newpage - before.newpage_updated.unwrap_or(0)),
            };
            (counts.inserted + counts.updated + counts.deleted > 0).then_some((relid, counts))
        })
        .collect();
    written.sort_by_key(|&(relid, _)| relid);
    let oids: Vec<u32> = written.iter().map(|&(relid, _)| relid).collect();
    let rows = client
        .query(&tables_query(server_version), &[&oids])
        .await?;
    let mut tables: Vec<TableWrites> = Vec::new();
    for (relid, counts) in written {
        let mut table: Option<TableWrites> = None;
        for row in rows.iter().filter(|row| row.get::<_, u32>(0) == relid) {
            let table = table.get_or_insert_with(|| TableWrites {
                table: QualifiedName::new(&row.get::<_, String>(1), &row.get::<_, String>(2)),
                inserted: counts.inserted,
                updated: counts.updated,
                deleted: counts.deleted,
                hot_updated: counts.hot_updated,
                newpage_updated: counts.newpage_updated,
                fillfactor: row
                    .get::<_, Option<i32>>(3)
                    .and_then(|fillfactor| u32::try_from(fillfactor).ok()),
                indexes: Vec::new(),
            });
            let Some(name) = row.get::<_, Option<String>>(4) else {
                continue;
            };
            table.indexes.push(IndexColumns {
                name: QualifiedName::new(&row.get::<_, String>(5), &name),
                columns: row.get(11),
                summarizing: row.get::<_, Option<String>>(6).as_deref() == Some("brin"),
                partial: row.get(7),
                enforces: row.get(8),
                inherited: row.get(9),
                scans: row.get(10),
            });
        }
        tables.extend(table);
    }
    Ok(WriteCapture {
        tables,
        server_version,
    })
}

/// Each table with its fillfactor, and one row for each of its indexes:
/// its access method, whether it is partial, enforces a constraint or
/// belongs to an index of a partitioned table, its scans, and every column
/// it refers to, in its keys and `INCLUDE` list (`indkey`) or in its
/// expressions and predicate (the `varattno` of each column in their node
/// trees).
fn tables_query(server_version: u32) -> String {
    let inherited = if server_version >= 110_000 {
        "i.relispartition"
    } else {
        "false"
    };
    format!(
        "
SELECT t.oid, tn.nspname::text, t.relname::text,
       (SELECT option_value FROM pg_catalog.pg_options_to_table(t.reloptions)
        WHERE option_name = 'fillfactor')::int4,
       i.relname::text, inn.nspname::text, am.amname::text,
       coalesce(x.indpred IS NOT NULL, false),
       coalesce(x.indisprimary OR x.indisunique OR x.indisexclusion, false),
       coalesce({inherited}, false),
       s.idx_scan,
       ARRAY(
           SELECT a.attname::text FROM pg_catalog.pg_attribute a
           WHERE a.attrelid = t.oid AND a.attnum > 0 AND NOT a.attisdropped
             AND (a.attnum = ANY (x.indkey::int2[])
                  OR a.attnum::text IN (
                      SELECT (regexp_matches(
                          coalesce(x.indexprs::text, '') || ' ' || coalesce(x.indpred::text, ''),
                          ':varattno (\\d+)', 'g'))[1]))
           ORDER BY a.attnum)
FROM pg_catalog.pg_class t
JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace
LEFT JOIN pg_catalog.pg_index x ON x.indrelid = t.oid
LEFT JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid
LEFT JOIN pg_catalog.pg_namespace inn ON inn.oid = i.relnamespace
LEFT JOIN pg_catalog.pg_am am ON am.oid = i.relam
LEFT JOIN pg_catalog.pg_stat_all_indexes s ON s.indexrelid = x.indexrelid
WHERE t.oid = ANY($1)
ORDER BY t.oid, i.relname"
    )
}

/// How long dropping an index may wait for its lock.
const LOCK_TIMEOUT: &str = "2s";

/// The statement run again without `indexes`, dropped in the same
/// transaction, which is rolled back with them: its measured plan and what
/// it wrote. Dropping an index locks its table against reads and writes
/// until the rollback, so `lock_timeout` gives up rather than wait.
pub(crate) async fn prove(
    client: &Client,
    sql: &str,
    indexes: &[QualifiedName],
    safety: Safety,
    server_version: u32,
) -> Result<(String, WriteCapture), Error> {
    if !safety.allow_ddl {
        return Err(Error::Refused(
            "dropping indexes to test the statement without them needs --allow-ddl".to_owned(),
        ));
    }
    let sql = exec::statement(sql)?;
    let server = |error: tokio_postgres::Error| Error::Server(describe(&error));
    let estimated = exec::explain(
        client,
        sql,
        exec::Mode::Estimate,
        &[],
        safety,
        server_version,
    )
    .await?;
    let writes = exec::allowed_writes(&estimated, safety)?;
    let explain = format!(
        "EXPLAIN ({}) {sql}",
        exec::analyze_options(server_version, writes)
    );
    let drops: String = indexes
        .iter()
        .map(|index| {
            format!(
                "DROP INDEX {}.{};",
                identifier(&index.schema),
                identifier(&index.name)
            )
        })
        .collect();
    client
        .batch_execute(if writes == Writes::No {
            "BEGIN READ ONLY"
        } else {
            "BEGIN"
        })
        .await
        .map_err(server)?;
    let result = async {
        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {}; SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'",
                safety.timeout.as_millis().max(1)
            ))
            .await?;
        client.batch_execute(&drops).await?;
        let observe = Observe {
            writes: true,
            ..Observe::nothing(server_version)
        };
        exec::in_transaction(client, observe, client.query(explain.as_str(), &[])).await
    }
    .await;
    // Always, whatever happened above: the indexes come back with it.
    let rollback = client.batch_execute("ROLLBACK").await;
    let (rows, Observed { writes, .. }) = result.map_err(|error| {
        let text = describe(&error);
        if text.contains("55P03") {
            Error::Server(format!(
                "the table is busy: dropping the index waited more than {LOCK_TIMEOUT} for its lock ({text})"
            ))
        } else {
            Error::Server(text)
        }
    })?;
    rollback.map_err(server)?;
    let row = rows
        .first()
        .ok_or_else(|| Error::Server("EXPLAIN returned no plan".to_owned()))?;
    let plan: serde_json::Value = row.try_get(0).map_err(server)?;
    let writes = writes
        .ok_or_else(|| Error::Server("what the statement wrote could not be read".to_owned()))?;
    Ok((plan.to_string(), writes))
}

/// A name in double quotes, safe whatever it holds.
fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_names() {
        assert_eq!(identifier("orders_idx"), "\"orders_idx\"");
        assert_eq!(
            identifier("a\"; DROP TABLE t; --"),
            "\"a\"\"; DROP TABLE t; --\""
        );
    }
}
