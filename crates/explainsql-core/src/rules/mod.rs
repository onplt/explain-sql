//! Rules turn plan data into findings. Every finding names its rule, the
//! node it is about, a severity derived from the share of the runtime
//! involved, the evidence that triggered it, and a suggested action. Each
//! rule lives in its own file and says when it deliberately stays silent: a
//! rule that is sometimes obviously wrong does more harm than a missing one.
//! The catalog is `docs/rules.md`.

mod es001_selective_seq_scan;
mod es002_row_misestimate;
mod es003_sort_spill;
mod es004_hash_spill;
mod es005_nested_loop_inner;
mod es006_index_scan_filter;
mod es007_heap_fetches;
mod es008_lossy_bitmap;
mod es009_foreign_key_trigger;
mod es010_cartesian_product;
mod es011_workers_not_launched;
mod es012_jit_overhead;

use serde::Serialize;

use crate::expr;
use crate::format;
use crate::ir::{Node, Plan, Relationship};
use crate::metrics::{self, Metrics, NodeMetrics};

/// A rule's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rule {
    /// A stable ID such as `ES001`.
    pub id: &'static str,
    pub name: &'static str,
}

/// Every rule, in ID order.
pub const RULES: [Rule; 12] = [
    es001_selective_seq_scan::RULE,
    es002_row_misestimate::RULE,
    es003_sort_spill::RULE,
    es004_hash_spill::RULE,
    es005_nested_loop_inner::RULE,
    es006_index_scan_filter::RULE,
    es007_heap_fetches::RULE,
    es008_lossy_bitmap::RULE,
    es009_foreign_key_trigger::RULE,
    es010_cartesian_product::RULE,
    es011_workers_not_launched::RULE,
    es012_jit_overhead::RULE,
];

type Check = fn(&Context) -> Vec<Finding>;

const CHECKS: [Check; 12] = [
    es001_selective_seq_scan::check,
    es002_row_misestimate::check,
    es003_sort_spill::check,
    es004_hash_spill::check,
    es005_nested_loop_inner::check,
    es006_index_scan_filter::check,
    es007_heap_fetches::check,
    es008_lossy_bitmap::check,
    es009_foreign_key_trigger::check,
    es010_cartesian_product::check,
    es011_workers_not_launched::check,
    es012_jit_overhead::check,
];

/// Something a rule found in a plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub rule: Rule,
    pub severity: Severity,
    /// The node the finding is about; `None` for the statement as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<crate::ir::NodeId>,
    /// What was found, in one sentence.
    pub summary: String,
    /// The figures behind it.
    pub evidence: Vec<Evidence>,
    /// What to do about it.
    pub action: String,
}

/// One labeled figure: `Rows read: 200,000`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence {
    pub label: &'static str,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    /// From the share of the runtime involved: half or more is high, a fifth
    /// or more medium. Unknown without timing.
    fn from_share(share: Option<f64>) -> Self {
        match share {
            Some(share) if share >= 0.5 => Severity::High,
            Some(share) if share >= 0.2 => Severity::Medium,
            Some(_) => Severity::Low,
            None => Severity::Medium,
        }
    }
}

/// Runs every rule. Findings come most severe first, then in rule order.
pub fn check(plan: &Plan, metrics: &Metrics) -> Vec<Finding> {
    let context = Context { plan, metrics };
    let mut findings: Vec<Finding> = CHECKS.iter().flat_map(|check| check(&context)).collect();
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.rule.id.cmp(b.rule.id))
            .then_with(|| a.node.cmp(&b.node))
    });
    findings
}

/// What rules look at.
struct Context<'a> {
    plan: &'a Plan,
    metrics: &'a Metrics,
}

impl<'a> Context<'a> {
    fn nodes(&self) -> impl Iterator<Item = &'a Node> {
        self.plan.nodes.iter()
    }

    fn metrics(&self, node: &Node) -> &'a NodeMetrics {
        self.metrics.node(node.id)
    }

    fn parent(&self, node: &Node) -> Option<&'a Node> {
        node.parent.map(|id| self.plan.node(id))
    }

    fn child(&self, node: &Node, relationship: Relationship) -> Option<&'a Node> {
        self.plan
            .children(node.id)
            .find(|child| child.relationship == Some(relationship))
    }

    /// The node's own share of the runtime: by exclusive time, or by
    /// exclusive buffers when the plan has no timing.
    fn share(&self, node: &Node) -> Option<f64> {
        let metrics = self.metrics(node);
        metrics.time_share.or_else(|| {
            let total = self
                .plan
                .root()
                .buffers
                .map(|buffers| metrics::blocks(&buffers))?;
            let own = metrics
                .exclusive_buffers
                .map(|buffers| metrics::blocks(&buffers))?;
            (total > 0).then(|| own as f64 / total as f64)
        })
    }

    /// The share of the runtime spent in the node and below it.
    fn inclusive_share(&self, node: &Node) -> Option<f64> {
        let total = self.metrics.statement.total_time?;
        self.metrics(node).inclusive_time.map(|time| time / total)
    }

    /// Whether the node matters for the statement's runtime: at least
    /// `minimum` of it, or unknown.
    fn on_hot_path(&self, node: &Node, minimum: f64) -> bool {
        self.share(node).is_none_or(|share| share >= minimum)
    }

    /// Evidence for the time spent in a node itself.
    fn time_evidence(&self, node: &Node) -> Option<Evidence> {
        let metrics = self.metrics(node);
        let time = metrics.exclusive_time?;
        let value = match metrics.time_share {
            Some(share) => format!(
                "{} ({} of the runtime)",
                format::duration(time),
                format::percent(share)
            ),
            None => format::duration(time),
        };
        Some(evidence("Time in the node", value))
    }
}

fn evidence(label: &'static str, value: impl Into<String>) -> Evidence {
    Evidence {
        label,
        value: value.into(),
    }
}

/// The relation a scan reads, for actions: `orders`.
pub(crate) fn relation(node: &Node) -> String {
    node.relation_name
        .clone()
        .or_else(|| node.alias.clone())
        .unwrap_or_else(|| "the table".to_owned())
}

/// The name scans use to qualify their columns: the alias, or the relation.
pub(crate) fn qualifier(node: &Node) -> Option<&str> {
    node.alias.as_deref().or(node.relation_name.as_deref())
}

/// Column names without their qualifiers, without duplicates: `a, b`.
fn column_list<'c>(columns: impl IntoIterator<Item = &'c str>) -> String {
    let mut names: Vec<&str> = Vec::new();
    for column in columns {
        let (_, name) = expr::split_column(column);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.join(", ")
}

/// The scan at the bottom of a chain of single-child nodes, such as the
/// Index Scan under a Limit. `None` when a cache (Materialize, Memoize)
/// is on the way, or the chain branches.
pub(crate) fn scan_below<'a>(plan: &'a Plan, node: &'a Node) -> Option<&'a Node> {
    let mut current = node;
    loop {
        if current.node_type.ends_with("Scan") {
            return Some(current);
        }
        if matches!(current.node_type.as_str(), "Materialize" | "Memoize") {
            return None;
        }
        let mut children = plan.children(current.id).filter(|child| {
            !matches!(
                child.relationship,
                Some(Relationship::InitPlan | Relationship::SubPlan)
            )
        });
        let only = children.next()?;
        if children.next().is_some() {
            return None;
        }
        current = only;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_in_id_order() {
        let ids: Vec<&str> = RULES.iter().map(|rule| rule.id).collect();
        let expected: Vec<String> = (1..=12).map(|n| format!("ES{n:03}")).collect();
        assert_eq!(ids, expected);
    }
}
