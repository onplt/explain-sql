//! ES008: a bitmap heap scan whose bitmap became lossy because it did not
//! fit in `work_mem`: for lossy pages only the page is remembered, so every
//! row on it is checked again.
//!
//! Silent when the scan is cheap compared with the statement. Rechecks
//! without lossy pages come from lossy operator classes, which `work_mem`
//! does not help, and are not reported.

use serde_json::Value;

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;
use crate::ir::Node;

pub const RULE: Rule = Rule {
    id: "ES008",
    name: "Lossy bitmap or heavy recheck",
};

const MIN_SHARE: f64 = 0.05;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type == "Bitmap Heap Scan")
        .filter_map(|node| lossy(context, node))
        .collect()
}

fn lossy(context: &Context, node: &Node) -> Option<Finding> {
    let lossy = node
        .extra
        .get("Lossy Heap Blocks")
        .and_then(Value::as_f64)
        .filter(|&blocks| blocks > 0.0)?;
    let share = context
        .inclusive_share(node)
        .or_else(|| context.share(node));
    if share.is_some_and(|share| share < MIN_SHARE) {
        return None;
    }
    let exact = node
        .extra
        .get("Exact Heap Blocks")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut facts = vec![evidence(
        "Heap blocks",
        format!(
            "{} lossy, {} exact",
            format::rows(lossy),
            format::rows(exact)
        ),
    )];
    if node.rows_removed_by_index_recheck > 0.0 {
        facts.push(evidence(
            "Rows removed by the recheck",
            format::rows(node.rows_removed_by_index_recheck),
        ));
    }
    facts.extend(context.time_evidence(node));
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(share),
        node: Some(node.id),
        summary: format!(
            "{} kept only page numbers for {} of {} pages, so it rechecked every row on them",
            format::node(node),
            format::rows(lossy),
            format::rows(lossy + exact)
        ),
        evidence: facts,
        action: "Raise work_mem for this query so that the bitmap stays exact.".to_owned(),
    })
}
