//! Suggestions checked against the database's catalog: an index that
//! already exists is explained instead of suggested again, a foreign key's
//! columns become a real `CREATE INDEX`, and caveats the catalog settles
//! (the collation, pg_trgm, a connection) are dropped.

use super::{Advice, AdviceKind, Index, IndexColumn, Method, NOT_CONNECTED};
use crate::catalog::{Catalog, Table};
use crate::format;

const PATTERN_OPS_CAVEAT: &str = "text_pattern_ops is needed";
const TRIGRAM_CAVEAT: &str = "gin_trgm_ops comes with the pg_trgm extension";

/// Refines advice made from the plan with what the catalog says.
pub fn refine(advice: &mut [Advice], catalog: &Catalog) {
    for item in advice.iter_mut() {
        item.caveats.retain(|caveat| caveat != NOT_CONNECTED);
        if let AdviceKind::ForeignKey { constraint } = &item.kind {
            let Some(key) = catalog
                .foreign_keys
                .iter()
                .find(|key| key.constraint == *constraint)
            else {
                continue;
            };
            let index = Index {
                schema: Some(key.schema.clone()),
                table: key.table.clone(),
                method: Method::Btree,
                columns: key
                    .columns
                    .iter()
                    .map(|name| IndexColumn {
                        name: name.clone(),
                        descending: false,
                        opclass: None,
                    })
                    .collect(),
                partitioned: false,
            };
            item.caveats.clear();
            item.summary = format!(
                "{} references its parent through {constraint}. An index on {} would let each foreign-key check find the referencing rows directly.",
                key.table,
                index.target()
            );
            item.kind = AdviceKind::Index {
                ddl: index.ddl(),
                index,
            };
        }
        let AdviceKind::Index { index, .. } = &mut item.kind else {
            continue;
        };
        let Some(table) = catalog.table(index.schema.as_deref(), &index.table) else {
            item.caveats.push(format!(
                "{} was not found in the database, so its existing indexes were not checked.",
                index.table
            ));
            continue;
        };
        index.schema = Some(table.schema.clone());
        index.partitioned = table.partitioned;
        settle_operator_classes(item, table, catalog);
        let AdviceKind::Index { index, .. } = &item.kind else {
            continue;
        };
        if let Some(existing) = covering_index(index, table) {
            let summary = already_indexed_summary(index, table, existing);
            item.kind = AdviceKind::AlreadyIndexed {
                index: existing.name.clone(),
                definition: existing.definition.clone(),
            };
            item.summary = summary;
            item.caveats.clear();
            continue;
        }
        let ddl = index.ddl();
        let size = format::kilobytes(table.total_bytes as f64 / 1024.0);
        item.caveats.push(format!(
            "Building it reads all of {} ({size} with its indexes). CONCURRENTLY does not block writes, but takes longer and cannot run inside a transaction.",
            table.name
        ));
        if let AdviceKind::Index { ddl: old, .. } = &mut item.kind {
            *old = ddl;
        }
    }
}

/// Drops operator classes and caveats the catalog makes unnecessary.
fn settle_operator_classes(item: &mut Advice, table: &Table, catalog: &Catalog) {
    let AdviceKind::Index { index, .. } = &mut item.kind else {
        return;
    };
    let mut changed = false;
    for column in &mut index.columns {
        if column.opclass == Some("text_pattern_ops") {
            let collation = table
                .column(&column.name)
                .and_then(|column| column.collation.as_deref());
            if matches!(collation, Some("C" | "POSIX")) {
                column.opclass = None;
                changed = true;
                item.caveats
                    .retain(|caveat| !caveat.starts_with(PATTERN_OPS_CAVEAT));
            }
        }
    }
    if catalog.extensions.iter().any(|name| name == "pg_trgm") {
        item.caveats
            .retain(|caveat| !caveat.starts_with(TRIGRAM_CAVEAT));
    }
    if changed {
        let ddl = index.ddl();
        if let AdviceKind::Index { ddl: old, .. } = &mut item.kind {
            *old = ddl;
        }
    }
}

/// A valid, complete index whose leading columns are the candidate's.
fn covering_index<'a>(
    index: &Index,
    table: &'a Table,
) -> Option<&'a crate::catalog::ExistingIndex> {
    let method = match index.method {
        Method::Btree => "btree",
        Method::Gin => "gin",
    };
    table.indexes.iter().find(|existing| {
        existing.valid
            && !existing.partial
            && existing.method == method
            && index.columns.len() <= existing.columns.len()
            && index
                .columns
                .iter()
                .zip(&existing.columns)
                .all(|(wanted, have)| wanted.name == *have)
            && index
                .columns
                .iter()
                .filter_map(|column| column.opclass)
                .all(|opclass| existing.definition.contains(opclass))
    })
}

fn already_indexed_summary(
    index: &Index,
    table: &Table,
    existing: &crate::catalog::ExistingIndex,
) -> String {
    let mut text = format!(
        "{} already serves {}, but the planner did not use it here.",
        existing.name,
        index.target()
    );
    match &table.last_analyzed {
        None => text.push_str(&format!(
            " {} has never been analyzed, so the planner's estimates may be far off: run ANALYZE {}.",
            table.name, table.name
        )),
        Some(when) => text.push_str(&format!(
            " The likely reasons: the condition compares the column with a value of another type or collation, or the planner expected more rows than it found (statistics from {when}; ANALYZE {} refreshes them).",
            table.name
        )),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Column, ExistingIndex, ForeignKey};

    fn analysis(text: &str) -> Vec<Advice> {
        let plan = crate::parse(text).unwrap();
        crate::analyze(&plan).advice
    }

    const SELECTIVE: &str = "\
Seq Scan on public.orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (orders.customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.900 ms";

    fn orders(indexes: Vec<ExistingIndex>) -> Catalog {
        Catalog {
            tables: vec![Table {
                schema: "public".to_owned(),
                name: "orders".to_owned(),
                pages: 2417.0,
                rows: 200_000.0,
                total_bytes: 30 * 1024 * 1024,
                last_analyzed: Some("2026-10-01 12:00".to_owned()),
                indexes,
                columns: vec![Column {
                    name: "note".to_owned(),
                    collation: Some("C".to_owned()),
                    n_distinct: None,
                }],
                ..Table::default()
            }],
            ..Catalog::default()
        }
    }

    #[test]
    fn explains_an_existing_index() {
        let mut advice = analysis(SELECTIVE);
        refine(
            &mut advice,
            &orders(vec![ExistingIndex {
                name: "orders_customer_id_status_idx".to_owned(),
                definition: "CREATE INDEX orders_customer_id_status_idx ON public.orders USING btree (customer_id, status)".to_owned(),
                method: "btree".to_owned(),
                columns: vec!["customer_id".to_owned(), "status".to_owned()],
                valid: true,
                ..ExistingIndex::default()
            }]),
        );
        assert!(
            matches!(&advice[0].kind, AdviceKind::AlreadyIndexed { index, .. } if index == "orders_customer_id_status_idx"),
            "{advice:?}"
        );
        // An invalid index left by a failed build does not count.
        let mut advice = analysis(SELECTIVE);
        refine(
            &mut advice,
            &orders(vec![ExistingIndex {
                name: "broken".to_owned(),
                method: "btree".to_owned(),
                columns: vec!["customer_id".to_owned()],
                valid: false,
                ..ExistingIndex::default()
            }]),
        );
        assert!(matches!(advice[0].kind, AdviceKind::Index { .. }));
        assert!(
            !advice[0]
                .caveats
                .iter()
                .any(|caveat| caveat == NOT_CONNECTED)
        );
        assert!(
            advice[0].caveats[0].contains("30.0 MB"),
            "{:?}",
            advice[0].caveats
        );
    }

    #[test]
    fn drops_pattern_ops_for_the_c_collation() {
        let mut advice = analysis(
            "\
Seq Scan on public.orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (orders.note ~~ 'ab%'::text)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.900 ms",
        );
        refine(&mut advice, &orders(Vec::new()));
        let AdviceKind::Index { ddl, .. } = &advice[0].kind else {
            panic!("{advice:?}");
        };
        assert_eq!(ddl, "CREATE INDEX CONCURRENTLY ON public.orders (note);");
    }

    #[test]
    fn turns_a_foreign_key_into_an_index() {
        let mut advice = analysis(
            "\
Delete on orders  (cost=0.42..8.77 rows=0 width=0) (actual time=0.098..0.098 rows=0 loops=1)
  ->  Index Scan using orders_pkey on orders  (cost=0.42..8.77 rows=20 width=6) (actual time=0.009..0.013 rows=20 loops=1)
        Index Cond: (id > 199980)
Planning Time: 0.333 ms
Trigger RI_ConstraintTrigger_a_16417 for constraint order_items_order_id_fkey: time=305.286 calls=20
Execution Time: 305.451 ms",
        );
        let catalog = Catalog {
            foreign_keys: vec![ForeignKey {
                constraint: "order_items_order_id_fkey".to_owned(),
                schema: "public".to_owned(),
                table: "order_items".to_owned(),
                columns: vec!["order_id".to_owned()],
            }],
            tables: vec![Table {
                schema: "public".to_owned(),
                name: "order_items".to_owned(),
                ..Table::default()
            }],
            ..Catalog::default()
        };
        refine(&mut advice, &catalog);
        let AdviceKind::Index { ddl, .. } = &advice[0].kind else {
            panic!("{advice:?}");
        };
        assert_eq!(
            ddl,
            "CREATE INDEX CONCURRENTLY ON public.order_items (order_id);"
        );
    }
}
