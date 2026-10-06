//! What identifies a node across plans of the same statement. Planned under
//! other settings or with another index, a statement's plan has other node
//! ids and often another shape, but a scan still reads the same relation,
//! under the same alias, and a join still combines the same relations.
//! These functions find the node that does the same work in another plan,
//! tell nodes that look alike but for numbers, and sum up what makes a
//! plan that plan: its shape.

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

/// The shape of a plan: what tells it apart from another plan of the same
/// statement. One line per node, in plan order: its operation (type, join
/// type, strategy, parallelism), what it reads (relation, index, CTE or
/// function, numbers blanked out, as in partition names) and the kinds of
/// its conditions. Costs, row counts, times, buffers and literal values are
/// left out, so the same plan has the same shape whatever the statement's
/// parameters, the data or the cache, and whether the plan was printed as
/// JSON or text. Aliases are left out too: PostgreSQL names partitions
/// differently from one version to the next (`events_2025_01`, then
/// `events_1`), and renaming an alias does not change a plan.
pub fn shape(plan: &Plan) -> String {
    let mut out = String::new();
    for (depth, node) in plan.walk() {
        out.push_str(&"  ".repeat(depth));
        shape_line(&mut out, node, node.relationship);
    }
    out
}

/// The [shape](shape) of a plan as it would be had the planner pruned its
/// partitions: an Append or Merge Append left with one child counts as that
/// child. A generic plan prunes partitions when it starts, with the values
/// it runs with, and keeps the Append that does it; a custom plan prunes
/// them when it is planned, and an Append with one child left goes.
pub fn pruned_shape(plan: &Plan) -> String {
    let mut out = String::new();
    pruned_lines(plan, plan.root().id, 0, None, &mut out);
    out
}

fn pruned_lines(
    plan: &Plan,
    id: NodeId,
    depth: usize,
    relationship: Option<Option<Relationship>>,
    out: &mut String,
) {
    let node = plan.node(id);
    let relationship = relationship.unwrap_or(node.relationship);
    if matches!(node.node_type.as_str(), "Append" | "Merge Append") && node.children.len() == 1 {
        return pruned_lines(plan, node.children[0], depth, Some(relationship), out);
    }
    out.push_str(&"  ".repeat(depth));
    shape_line(out, node, relationship);
    for &child in &node.children {
        pruned_lines(plan, child, depth + 1, None, out);
    }
}

/// A node's line of a shape, with the relationship to its parent given.
fn shape_line(out: &mut String, node: &Node, relationship: Option<Relationship>) {
    out.push_str(&node.node_type);
    let mut field = |name: &str, value: Option<&str>| {
        if let Some(value) = value {
            out.push_str(&format!(" {name}={}", blank_numbers(value)));
        }
    };
    field("parallel", node.parallel_aware.then_some("yes"));
    field("join", node.join_type.as_deref());
    field("strategy", node.strategy.as_deref());
    field("partial", node.partial_mode.as_deref());
    field("operation", node.operation.as_deref());
    field("command", node.command.as_deref());
    field("direction", node.scan_direction.as_deref());
    field(
        "relationship",
        relationship.map(|relationship| match relationship {
            Relationship::Outer => "outer",
            Relationship::Inner => "inner",
            Relationship::Member => "member",
            Relationship::InitPlan => "initplan",
            Relationship::SubPlan => "subplan",
            Relationship::Subquery => "subquery",
        }),
    );
    field("relation", node.relation_name.as_deref());
    field("index", node.index_name.as_deref());
    field("cte", node.cte_name.as_deref());
    field("function", node.function_name.as_deref());
    field("provider", node.custom_plan_provider.as_deref());
    let kinds: BTreeSet<&str> = node
        .predicates
        .iter()
        .map(|predicate| predicate.kind.key())
        .collect();
    for kind in kinds {
        out.push_str(&format!(" [{kind}]"));
    }
    out.push('\n');
}

/// The [shape](shape) of a plan as 16 hexadecimal digits (64-bit FNV-1a),
/// to tell at a glance whether two plans are the same.
pub fn id(plan: &Plan) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in shape(plan).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
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
    fn the_same_plan_has_the_same_shape_whatever_its_numbers() {
        let plan = crate::parse(JOIN).unwrap();
        // Other costs, row counts, times and literal values.
        let measured = crate::parse(
            "\
Hash Join  (cost=3.00..30.00 rows=99 width=8) (actual time=0.100..0.900 rows=12 loops=1)
  Hash Cond: (o.customer_id = c.id)
  ->  Seq Scan on orders o  (cost=0.00..5.00 rows=100 width=4) (actual time=0.010..0.500 rows=80 loops=1)
        Filter: (status = 'shipped'::text)
        Rows Removed by Filter: 20
  ->  Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=0.050..0.050 rows=10 loops=1)
        Buckets: 1024  Batches: 1  Memory Usage: 9kB
        ->  Bitmap Heap Scan on customers c  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.040 rows=10 loops=1)
              Recheck Cond: (country = 'DE'::text)
              ->  Bitmap Index Scan on customers_country_idx  (cost=0.00..1.00 rows=10 width=0) (actual time=0.005..0.005 rows=10 loops=1)
                    Index Cond: (country = 'DE'::text)",
        )
        .unwrap();
        assert_eq!(shape(&plan), shape(&measured));
        assert_eq!(id(&plan), id(&measured));
        assert_eq!(id(&plan).len(), 16);
        assert!(shape(&plan).starts_with(
            "Hash Join join=Inner [Hash Cond]\n  Seq Scan relationship=outer relation=orders [Filter]\n"
        ));
        // Another plan of the statement.
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
        assert_ne!(id(&plan), id(&other));
    }

    #[test]
    fn a_plan_pruned_as_it_starts_has_the_shape_of_one_pruned_as_planned() {
        // The generic plan, run with values that leave one partition.
        let generic = crate::parse(
            "\
Append  (cost=4.81..1183.72 rows=599 width=63)
  Subplans Removed: 11
  ->  Bitmap Heap Scan on events_2025_03 events_1  (cost=4.81..100.67 rows=51 width=63)
        Recheck Cond: ((created_at >= $1) AND (created_at < $2))
        ->  Bitmap Index Scan on events_2025_03_created_at_idx  (cost=0.00..4.79 rows=51 width=0)
              Index Cond: ((created_at >= $1) AND (created_at < $2))",
        )
        .unwrap();
        let custom = crate::parse(
            "\
Bitmap Heap Scan on events_2025_03 events  (cost=11.80..137.95 rows=343 width=63)
  Recheck Cond: ((created_at >= '2025-03-01 00:00:00+00'::timestamp with time zone) AND (created_at < '2025-03-02 00:00:00+00'::timestamp with time zone))
  ->  Bitmap Index Scan on events_2025_03_created_at_idx  (cost=0.00..11.71 rows=343 width=0)
        Index Cond: ((created_at >= '2025-03-01 00:00:00+00'::timestamp with time zone) AND (created_at < '2025-03-02 00:00:00+00'::timestamp with time zone))",
        )
        .unwrap();
        assert_ne!(shape(&generic), shape(&custom));
        assert_eq!(pruned_shape(&generic), pruned_shape(&custom));
        // An Append of several partitions stays.
        let plan = crate::parse(JOIN).unwrap();
        assert_eq!(pruned_shape(&plan), shape(&plan));
        let two = crate::parse(
            "\
Append  (cost=4.81..1183.72 rows=599 width=63)
  Subplans Removed: 10
  ->  Seq Scan on events_2025_03 events_1  (cost=0.00..100.67 rows=51 width=63)
  ->  Seq Scan on events_2025_04 events_2  (cost=0.00..96.71 rows=49 width=63)",
        )
        .unwrap();
        assert!(pruned_shape(&two).starts_with("Append\n"));
    }

    #[test]
    fn blanks_numbers() {
        assert_eq!(blank_numbers("events_2025_01"), "events_#_#");
        assert_eq!(blank_numbers("(n = 42)"), "(n = #)");
    }
}
