//! ES007: an index-only scan that had to visit the table for many of its
//! rows, because the visibility map does not mark their pages all-visible.
//!
//! Silent when the heap fetches are few, and when the scan is cheap compared
//! with the statement.

use serde_json::Value;

use super::{Context, Finding, Rule, Severity, evidence, relation};
use crate::format;
use crate::ir::Node;

pub const RULE: Rule = Rule {
    id: "ES007",
    name: "Index-only scan with many heap fetches",
};

/// Heap fetches per row returned.
const MIN_FETCH_RATIO: f64 = 0.25;
/// Heap fetches over all loops.
const MIN_FETCHES: f64 = 100.0;
const MIN_SHARE: f64 = 0.1;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type == "Index Only Scan")
        .filter_map(|node| heap_fetches(context, node))
        .collect()
}

fn heap_fetches(context: &Context, node: &Node) -> Option<Finding> {
    let actuals = node.actuals.filter(|actuals| !actuals.never_executed())?;
    // Like rows, heap fetches are printed per loop.
    let fetches = node.extra.get("Heap Fetches").and_then(Value::as_f64)?;
    let loops = actuals.loops as f64;
    if fetches * loops < MIN_FETCHES || fetches < MIN_FETCH_RATIO * actuals.rows.max(1.0) {
        return None;
    }
    if !context.on_hot_path(node, MIN_SHARE) {
        return None;
    }
    let table = relation(node);
    let mut facts = vec![
        evidence("Heap fetches", format::rows(fetches * loops)),
        evidence("Rows returned", format::rows(actuals.rows * loops)),
    ];
    facts.extend(context.time_evidence(node));
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(context.share(node)),
        node: Some(node.id),
        summary: format!(
            "{} visited the table {} times for {} rows: the visibility map of {table} is out of date",
            format::node(node),
            format::rows(fetches * loops),
            format::rows(actuals.rows * loops)
        ),
        evidence: facts,
        action: format!(
            "VACUUM {table} to update its visibility map, and check that autovacuum keeps up with how often the table changes."
        ),
    })
}
