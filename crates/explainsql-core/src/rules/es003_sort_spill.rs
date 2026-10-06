//! ES003: a sort that did not fit in `work_mem` and spilled to disk
//! (`external merge` or `external sort`), in the leader or in a parallel
//! worker.
//!
//! Silent when the spill is small and the sort is cheap compared with the
//! statement.

use serde_json::Value;

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;
use crate::ir::Node;
use crate::memory;

pub const RULE: Rule = Rule {
    id: "ES003",
    name: "Sort spilled to disk",
};

/// Spills from this size (in kB) are reported whatever their cost.
const MIN_DISK_KB: f64 = 10.0 * 1024.0;
/// Share of the runtime from which a smaller spill is reported.
const MIN_SHARE: f64 = 0.05;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| matches!(node.node_type.as_str(), "Sort" | "Incremental Sort"))
        .filter_map(|node| spill(context, node))
        .collect()
}

fn spill(context: &Context, node: &Node) -> Option<Finding> {
    // The node's own figures, then its workers'.
    let disk = std::iter::once(&node.extra)
        .chain(node.workers.iter().map(|worker| &worker.extra))
        .filter(|extra| on_disk(extra))
        .map(|extra| {
            extra
                .get("Sort Space Used")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        })
        .fold(None, |largest: Option<f64>, kb| {
            Some(largest.map_or(kb, |largest| largest.max(kb)))
        })?;
    if disk < MIN_DISK_KB && !context.on_hot_path(node, MIN_SHARE) {
        return None;
    }
    let method = node
        .extra
        .get("Sort Method")
        .and_then(Value::as_str)
        .unwrap_or("external merge");
    let mut facts = vec![
        evidence("Sort method", method),
        evidence("Disk used", format::kilobytes(disk)),
    ];
    if !node.sort_key.is_empty() {
        facts.push(evidence("Sort key", node.sort_key.join(", ")));
    }
    facts.extend(context.time_evidence(node));
    let mut action = match (
        memory::advice(context.plan, context.metrics, node),
        memory::needed(node),
    ) {
        (Some(advice), _) => format!("Raise work_mem so that the sort fits in memory. {advice}"),
        #[allow(clippy::cast_precision_loss)]
        (None, Some(needed)) if needed > memory::MAX_WORK_MEM as f64 => format!(
            "In memory, the sort would take about {}, more than work_mem should allow: sort fewer rows.",
            format::kilobytes(needed)
        ),
        _ => format!(
            "Raise work_mem for this statement rather than for the server (SET LOCAL work_mem), to about three times the {} written to disk.",
            format::kilobytes(disk)
        ),
    };
    if !node.sort_key.is_empty() {
        action.push_str(&format!(
            " Or avoid the sort with an index that returns rows ordered by {}.",
            node.sort_key.join(", ")
        ));
    }
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(context.share(node)),
        node: Some(node.id),
        summary: format!(
            "{} spilled {} to disk",
            format::node(node),
            format::kilobytes(disk)
        ),
        evidence: facts,
        action,
    })
}

fn on_disk(extra: &std::collections::BTreeMap<String, Value>) -> bool {
    extra.get("Sort Space Type").and_then(Value::as_str) == Some("Disk")
        || extra
            .get("Sort Method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.starts_with("external"))
}
