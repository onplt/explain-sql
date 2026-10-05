//! Reads what the advisor needs about the tables in a plan, and nothing
//! else: their size, indexes, column collations and statistics, the
//! foreign keys it names, and the installed extensions.

use explainsql_core::catalog::{Catalog, Column, ExistingIndex, ForeignKey, Table};
use tokio_postgres::Client;

use crate::{Error, Safety, describe};

const TABLE: &str = "
SELECT c.oid, n.nspname::text, c.relname::text, c.relkind = 'p',
       c.relpages::float8, c.reltuples::float8,
       pg_total_relation_size(c.oid)::int8,
       greatest(s.last_analyze, s.last_autoanalyze)::text
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_stat_all_tables s ON s.relid = c.oid
WHERE c.oid = to_regclass(CASE WHEN $1::text IS NULL THEN quote_ident($2::text)
                               ELSE format('%I.%I', $1::text, $2::text) END)";

const INDEXES: &str = "
SELECT i.relname::text, pg_get_indexdef(x.indexrelid), am.amname::text,
       x.indisunique, x.indisvalid, x.indpred IS NOT NULL,
       ARRAY(SELECT pg_get_indexdef(x.indexrelid, k, true)
             FROM generate_series(1, x.indnkeyatts::int) k)
FROM pg_index x
JOIN pg_class i ON i.oid = x.indexrelid
JOIN pg_am am ON am.oid = i.relam
WHERE x.indrelid = $1
ORDER BY i.relname";

const COLUMNS: &str = "
SELECT a.attname::text, co.collname::text, s.n_distinct::float8
FROM pg_attribute a
LEFT JOIN pg_collation co ON co.oid = a.attcollation
LEFT JOIN pg_stats s ON s.schemaname = $2 AND s.tablename = $3 AND s.attname = a.attname
WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum";

const FOREIGN_KEYS: &str = "
SELECT c.conname::text, n.nspname::text, t.relname::text,
       ARRAY(SELECT a.attname::text
             FROM unnest(c.conkey) WITH ORDINALITY k(attnum, position)
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
             ORDER BY k.position)
FROM pg_constraint c
JOIN pg_class t ON t.oid = c.conrelid
JOIN pg_namespace n ON n.oid = t.relnamespace
WHERE c.contype = 'f' AND c.conname = ANY($1)";

pub(crate) async fn read(
    client: &Client,
    tables: &[(Option<String>, String)],
    constraints: &[String],
    safety: Safety,
) -> Result<Catalog, Error> {
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
        read_all(client, tables, constraints).await
    }
    .await;
    let rollback = client.batch_execute("ROLLBACK").await;
    let catalog = result.map_err(server)?;
    rollback.map_err(server)?;
    Ok(catalog)
}

async fn read_all(
    client: &Client,
    wanted: &[(Option<String>, String)],
    constraints: &[String],
) -> Result<Catalog, tokio_postgres::Error> {
    let mut catalog = Catalog::default();
    // Foreign keys first: their referencing tables are wanted too.
    let mut wanted = wanted.to_vec();
    if !constraints.is_empty() {
        for row in client.query(FOREIGN_KEYS, &[&constraints]).await? {
            catalog.foreign_keys.push(ForeignKey {
                constraint: row.get(0),
                schema: row.get(1),
                table: row.get(2),
                columns: row.get(3),
            });
            let key = catalog.foreign_keys.last().expect("just pushed");
            wanted.push((Some(key.schema.clone()), key.table.clone()));
        }
    }
    for (schema, name) in &wanted {
        let Some(row) = client.query_opt(TABLE, &[schema, name]).await? else {
            continue;
        };
        let oid: u32 = row.get(0);
        let mut table = Table {
            schema: row.get(1),
            name: row.get(2),
            partitioned: row.get(3),
            pages: row.get(4),
            rows: row.get(5),
            total_bytes: row.get(6),
            last_analyzed: row.get(7),
            ..Table::default()
        };
        if catalog
            .tables
            .iter()
            .any(|other| other.schema == table.schema && other.name == table.name)
        {
            continue;
        }
        for row in client.query(INDEXES, &[&oid]).await? {
            table.indexes.push(ExistingIndex {
                name: row.get(0),
                definition: row.get(1),
                method: row.get(2),
                unique: row.get(3),
                valid: row.get(4),
                partial: row.get(5),
                columns: row.get(6),
            });
        }
        for row in client
            .query(COLUMNS, &[&oid, &table.schema, &table.name])
            .await?
        {
            table.columns.push(Column {
                name: row.get(0),
                collation: row.get(1),
                n_distinct: row.get(2),
            });
        }
        catalog.tables.push(table);
    }
    for row in client
        .query("SELECT extname::text FROM pg_extension ORDER BY 1", &[])
        .await?
    {
        catalog.extensions.push(row.get(0));
    }
    Ok(catalog)
}
