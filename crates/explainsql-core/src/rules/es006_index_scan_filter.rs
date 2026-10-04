//! ES006: an index scan (or bitmap heap scan) whose filter throws away most
//! of the rows the index found: the index narrows the search by one
//! condition, and the rest of the work is done row by row.
//!
//! Silent when few rows are removed, and when the scan is cheap compared
//! with the statement.

use super::predicate::{self, Access};
use super::{Context, Finding, Rule, Severity, column_list, evidence, relation};
use crate::format;
use crate::ir::{Node, PredicateKind};

pub const RULE: Rule = Rule {
    id: "ES006",
    name: "Index scan that filters most rows",
};

/// Fraction of the rows found through the index that the filter removes.
const MIN_REMOVED: f64 = 0.9;
/// Rows removed per loop, unless the loops remove `MIN_TOTAL_REMOVED`.
const MIN_REMOVED_ROWS: f64 = 100.0;
const MIN_TOTAL_REMOVED: f64 = 1000.0;
/// Share of the runtime spent in the scan, including its index.
const MIN_SHARE: f64 = 0.1;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| {
            matches!(
                node.node_type.as_str(),
                "Index Scan" | "Index Only Scan" | "Bitmap Heap Scan"
            )
        })
        .filter_map(|node| filtering_scan(context, node))
        .collect()
}

fn filtering_scan(context: &Context, node: &Node) -> Option<Finding> {
    let actuals = node.actuals.filter(|actuals| !actuals.never_executed())?;
    let filter = node.predicate(PredicateKind::Filter)?;
    let removed = node.rows_removed_by_filter;
    let loops = actuals.loops as f64;
    if removed < MIN_REMOVED_ROWS && removed * loops < MIN_TOTAL_REMOVED {
        return None;
    }
    let found = removed + actuals.rows;
    if removed / found < MIN_REMOVED {
        return None;
    }
    let share = context
        .inclusive_share(node)
        .or_else(|| context.share(node));
    if share.is_some_and(|share| share < MIN_SHARE) {
        return None;
    }

    // The index and its condition: on the node, or on the Bitmap Index Scan below.
    let index_node = if node.node_type == "Bitmap Heap Scan" {
        context
            .plan
            .children(node.id)
            .find(|child| child.index_name.is_some())
            .unwrap_or(node)
    } else {
        node
    };
    let index = index_node.index_name.as_deref().unwrap_or("the index");
    let index_condition = index_node
        .predicate(PredicateKind::IndexCond)
        .or(node.predicate(PredicateKind::RecheckCond));

    let mut facts = vec![evidence("Index", index)];
    if let Some(condition) = index_condition {
        facts.push(evidence("Index condition", condition));
    }
    facts.push(evidence("Filter", filter));
    facts.push(evidence(
        "Rows removed by the filter",
        format!(
            "{} of {} found{}",
            format::rows(removed),
            format::rows(found),
            if loops > 1.0 { " per loop" } else { "" }
        ),
    ));
    if loops > 1.0 {
        facts.push(evidence("Loops", format::rows(loops)));
    }

    let filter_columns: Vec<&str> = predicate::conjuncts(filter)
        .into_iter()
        .filter_map(|conjunct| match predicate::access(conjunct) {
            Access::Column { column, .. } => Some(column),
            // A join condition pushed into the scan: the scan's own column.
            Access::Columns(a, b) => [a, b]
                .into_iter()
                .find(|column| predicate::split_column(column).0 == super::qualifier(node)),
            _ => None,
        })
        .collect();
    let index_columns: Vec<&str> = index_condition
        .map(|condition| {
            predicate::conjuncts(condition)
                .into_iter()
                .filter_map(|conjunct| match predicate::access(conjunct) {
                    Access::Column { column, .. } => Some(column),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let table = relation(node);
    let action = if filter_columns.is_empty() {
        format!(
            "A composite index on {table} that also covers the filtered columns would let the index do the filtering."
        )
    } else if index_columns.is_empty() {
        format!(
            "An index on {table} that starts with {} and continues with the columns of {index} would let the index do the filtering.",
            column_list(filter_columns)
        )
    } else {
        format!(
            "A composite index on {table} ({}) would let the index do the filtering.",
            column_list(filter_columns.into_iter().chain(index_columns))
        )
    };
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(share),
        node: Some(node.id),
        summary: format!(
            "{} finds {} rows through {index} and its filter throws away {}",
            format::node(node),
            format::rows(found * loops),
            format::rows(removed * loops)
        ),
        evidence: facts,
        action,
    })
}
