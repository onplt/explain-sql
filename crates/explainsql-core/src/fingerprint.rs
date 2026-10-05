//! What identifies a node across plans of the same statement. Planned under
//! other settings or with another index, a statement's plan has other node
//! ids and often another shape, but a scan still reads the same relation,
//! under the same alias, and a join still combines the same relations.
//! These functions find the node that does the same work in another plan,
//! and tell nodes that look alike but for numbers.

use std::collections::BTreeSet;

use crate::ir::{Node, NodeId, Plan, PredicateKind, Relationship};

/// A relation as a plan's scans read it: its name and its alias, which
/// tells apart two scans of one table in a self-join.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Relation {
    pub name: String,
    pub alias: Option<String>,
}

impl Relation {
    /// The relation a scan reads. Data-modifying nodes name a relation too,
    /// but do not scan it.
    pub fn of(node: &Node) -> Option<Relation> {
        if node.node_type == "ModifyTable" {
            return None;
        }
        Some(Relation {
            name: node.relation_name.clone()?,
            alias: node.alias.clone(),
        })
    }
}

/// The relations read at or below a node, leaving out InitPlans and
/// SubPlans, which the planner plans on their own.
pub fn relations(plan: &Plan, id: NodeId) -> BTreeSet<Relation> {
    let mut found = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        let node = plan.node(id);
        found.extend(Relation::of(node));
        stack.extend(
            plan.children(id)
                .filter(|child| {
                    !matches!(
                        child.relationship,
                        Some(Relationship::InitPlan | Relationship::SubPlan)
                    )
                })
                .map(|child| child.id),
        );
    }
    found
}

/// The node that reads a relation in a plan, the first in plan order.
pub fn find_scan<'a>(plan: &'a Plan, relation: &Relation) -> Option<&'a Node> {
    plan.walk()
        .into_iter()
        .map(|(_, node)| node)
        .find(|node| Relation::of(node).as_ref() == Some(relation))
}

const JOINS: [&str; 3] = ["Nested Loop", "Hash Join", "Merge Join"];

pub fn is_join(node: &Node) -> bool {
    JOINS.contains(&node.node_type.as_str())
}

/// The join that combines exactly these relations.
pub fn find_join<'a>(plan: &'a Plan, wanted: &BTreeSet<Relation>) -> Option<&'a Node> {
    plan.nodes
        .iter()
        .filter(|node| is_join(node))
        .find(|node| relations(plan, node.id) == *wanted)
}

/// The indexes a scan narrows its search with: its own index condition, or
/// the bitmap index scans below a Bitmap Heap Scan. Empty for a scan that
/// reads a whole index, in its order, without a condition.
pub fn indexes_with_condition(plan: &Plan, scan: &Node) -> Vec<String> {
    let mut indexes = Vec::new();
    let mut stack = vec![scan.id];
    while let Some(id) = stack.pop() {
        let node = plan.node(id);
        if let Some(index) = &node.index_name {
            if node.predicate(PredicateKind::IndexCond).is_some() && !indexes.contains(index) {
                indexes.push(index.clone());
            }
        }
        // A Bitmap Heap Scan reads the bitmaps its children build.
        if node.id == scan.id && node.node_type != "Bitmap Heap Scan" {
            continue;
        }
        stack.extend(node.children.iter().rev());
    }
    indexes
}

/// Whether a node reads an index in full: an index scan with no condition,
/// chosen for the order it returns rows in or because nothing else was
/// allowed.
pub fn reads_whole_index(node: &Node) -> bool {
    matches!(node.node_type.as_str(), "Index Scan" | "Index Only Scan")
        && node.predicate(PredicateKind::IndexCond).is_none()
}

/// What makes leaves similar, as in the scans of a thousand partitions:
/// their type, relation and conditions with numbers blanked out
/// (`orders_2025_01`, `events_1`). `None` for nodes with children.
pub fn leaf_key(plan: &Plan, id: NodeId) -> Option<String> {
    let node = plan.node(id);
    if !node.children.is_empty() {
        return None;
    }
    let mut key = format!(
        "{}|{}",
        node.node_type,
        node.relation_name.as_deref().unwrap_or("")
    );
    for predicate in &node.predicates {
        key.push('|');
        key.push_str(&predicate.text);
    }
    Some(blank_numbers(&key))
}

/// Each run of digits as a single `#`.
pub fn blank_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_number = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_number {
                out.push('#');
            }
            in_number = true;
        } else {
            in_number = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN: &str = "\
Hash Join  (cost=1.00..10.00 rows=10 width=8)
  Hash Cond: (o.customer_id = c.id)
  ->  Seq Scan on orders o  (cost=0.00..5.00 rows=100 width=4)
        Filter: (status = 'pending'::text)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4)
        ->  Bitmap Heap Scan on customers c  (cost=0.00..1.00 rows=10 width=4)
              Recheck Cond: (country = 'TR'::text)
              ->  Bitmap Index Scan on customers_country_idx  (cost=0.00..1.00 rows=10 width=0)
                    Index Cond: (country = 'TR'::text)";

    #[test]
    fn finds_the_same_scan_and_join_in_another_plan() {
        let plan = crate::parse(JOIN).unwrap();
        let orders = Relation {
            name: "orders".to_owned(),
            alias: Some("o".to_owned()),
        };
        assert_eq!(find_scan(&plan, &orders).unwrap().id, NodeId(1));
        let both = relations(&plan, NodeId(0));
        assert_eq!(both.len(), 2);
        assert_eq!(find_join(&plan, &both).unwrap().id, NodeId(0));
        // Another plan of the same statement.
        let other = crate::parse(
            "\
Nested Loop  (cost=0.29..20.00 rows=10 width=8)
  ->  Seq Scan on customers c  (cost=0.00..1.00 rows=10 width=4)
        Filter: (country = 'TR'::text)
  ->  Index Scan using orders_customer_id_idx on orders o  (cost=0.29..1.50 rows=1 width=4)
        Index Cond: (customer_id = c.id)
        Filter: (status = 'pending'::text)",
        )
        .unwrap();
        assert_eq!(find_join(&other, &both).unwrap().node_type, "Nested Loop");
        let scan = find_scan(&other, &orders).unwrap();
        assert_eq!(
            indexes_with_condition(&other, scan),
            ["orders_customer_id_idx"]
        );
    }

    #[test]
    fn tells_index_conditions_from_whole_index_reads() {
        let plan = crate::parse(JOIN).unwrap();
        assert_eq!(
            indexes_with_condition(&plan, plan.node(NodeId(3))),
            ["customers_country_idx"]
        );
        assert!(indexes_with_condition(&plan, plan.node(NodeId(1))).is_empty());
        let whole = crate::parse(
            "Index Scan using orders_created_at_idx on orders  (cost=0.42..9000.00 rows=10 width=64)\n  Filter: (date_trunc('day'::text, created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)",
        )
        .unwrap();
        assert!(reads_whole_index(whole.root()));
        assert!(indexes_with_condition(&whole, whole.root()).is_empty());
    }

    #[test]
    fn blanks_numbers() {
        assert_eq!(blank_numbers("events_2025_01"), "events_#_#");
        assert_eq!(blank_numbers("(n = 42)"), "(n = #)");
    }
}
