//! ES004: a hash table that did not fit in memory: a `Hash` split into
//! several batches, or a hashed aggregate that wrote partitions to disk.
//!
//! When the input of the hash was underestimated (ES002), the planner sized
//! it for fewer rows, and the action says so.

use serde_json::Value;

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;
use crate::ir::Node;

pub const RULE: Rule = Rule {
    id: "ES004",
    name: "Hash or aggregate spilled to disk",
};

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter_map(|node| match node.node_type.as_str() {
            "Hash" => hash(context, node),
            "Aggregate" if matches!(node.strategy.as_deref(), Some("Hashed" | "Mixed")) => {
                aggregate(context, node)
            }
            _ => None,
        })
        .collect()
}

fn number(node: &Node, key: &str) -> Option<f64> {
    node.extra.get(key).and_then(Value::as_f64)
}

fn hash(context: &Context, node: &Node) -> Option<Finding> {
    let batches = number(node, "Hash Batches").filter(|&batches| batches > 1.0)?;
    let original = number(node, "Original Hash Batches").unwrap_or(batches);
    let mut facts = vec![evidence(
        "Batches",
        if original < batches {
            format!(
                "{} (planned {})",
                format::rows(batches),
                format::rows(original)
            )
        } else {
            format::rows(batches)
        },
    )];
    if let Some(memory) = number(node, "Peak Memory Usage") {
        facts.push(evidence("Memory used", format::kilobytes(memory)));
    }
    if let Some(buffers) = node.buffers.filter(|buffers| buffers.temp_written > 0) {
        facts.push(evidence(
            "Written to disk",
            format::pages(buffers.temp_written as f64),
        ));
    }
    // The cost shows in the join that probes the hash table as well.
    let join = context
        .parent(node)
        .filter(|parent| parent.node_type == "Hash Join");
    let share = join
        .and_then(|join| context.inclusive_share(join))
        .or(context.share(node));
    let mut action = "Raise work_mem (or hash_mem_multiplier) for this query so that the hash table fits in memory".to_owned();
    match number(node, "Peak Memory Usage") {
        // Each batch held about as much as the one in memory.
        Some(memory) => action.push_str(&format!(
            ": the whole table takes roughly {}, {} batches of {}.",
            format::kilobytes(memory * batches),
            format::rows(batches),
            format::kilobytes(memory)
        )),
        None => action.push('.'),
    }
    if underestimated_input(context, node) {
        action.push_str(" The planner expected far fewer rows from the input (ES002), so fixing that estimate may let it size the hash correctly.");
    }
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(share),
        node: Some(node.id),
        summary: format!(
            "{} was split into {} batches because it did not fit in work_mem",
            join.map_or_else(
                || format::node(node),
                |join| format!("The hash table of {}", format::node(join))
            ),
            format::rows(batches)
        ),
        evidence: facts,
        action,
    })
}

fn aggregate(context: &Context, node: &Node) -> Option<Finding> {
    let batches = number(node, "HashAgg Batches").unwrap_or(0.0);
    let disk = number(node, "Disk Usage").unwrap_or(0.0);
    if batches <= 1.0 && disk <= 0.0 {
        return None;
    }
    let mut facts = Vec::new();
    if batches > 1.0 {
        facts.push(evidence("Batches", format::rows(batches)));
    }
    if let Some(memory) = number(node, "Peak Memory Usage") {
        facts.push(evidence("Memory used", format::kilobytes(memory)));
    }
    if disk > 0.0 {
        facts.push(evidence("Disk used", format::kilobytes(disk)));
    }
    facts.extend(context.time_evidence(node));
    let mut action = "Raise work_mem (or hash_mem_multiplier) for this query so that the aggregate's hash table fits in memory.".to_owned();
    if underestimated_input(context, node) {
        action.push_str(" The planner expected far fewer rows from the input (ES002).");
    }
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(context.share(node)),
        node: Some(node.id),
        summary: format!(
            "{} wrote {} to disk because its hash table did not fit in work_mem",
            format::node(node),
            format::kilobytes(disk)
        ),
        evidence: facts,
        action,
    })
}

fn underestimated_input(context: &Context, node: &Node) -> bool {
    context.plan.children(node.id).any(|child| {
        context
            .metrics(child)
            .misestimate
            .is_some_and(|error| error.underestimated && error.factor >= 10.0)
    })
}
