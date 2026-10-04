//! ES011: a Gather that started fewer parallel workers than planned, because
//! the pool of worker processes was exhausted when the query ran. The
//! leader then did the missing workers' share itself.

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;

pub const RULE: Rule = Rule {
    id: "ES011",
    name: "Fewer parallel workers than planned",
};

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type.starts_with("Gather"))
        .filter_map(|node| {
            let (planned, launched) = (node.workers_planned?, node.workers_launched?);
            (launched < planned).then(|| Finding {
                rule: RULE,
                severity: Severity::from_share(context.inclusive_share(node)),
                node: Some(node.id),
                summary: format!(
                    "{} started {launched} of the {planned} parallel workers it planned",
                    format::node(node)
                ),
                evidence: vec![
                    evidence("Workers planned", planned.to_string()),
                    evidence("Workers launched", launched.to_string()),
                ],
                action: "The pool of parallel workers was exhausted: check max_parallel_workers and max_worker_processes against the number of parallel queries running at once.".to_owned(),
            })
        })
        .collect()
}
