//! Two plans of the same statement, node by node: which scans read their
//! relation another way, which joins changed method or order, which nodes
//! came or went, and how the work of the nodes in both plans changed.
//!
//! Nodes are matched by the work they do, not by where they sit. A scan is
//! found again by the relation and alias it reads, or by the relation alone
//! (partitions get their aliases in plan order, so pruning renumbers them).
//! A join is found by the relations it combines, and any other node by its
//! kind of operation and the relations below it, with partition numbers
//! blanked out, so that an Append still matches when pruning leaves fewer
//! partitions. When several nodes share a key, they match in plan order.
//!
//! Changes come with the most significant first: another way to read a
//! relation, another join method or order, another strategy, nodes that
//! came or went, spills to disk; then misestimates and matched nodes whose
//! work grew or shrank by more than the noise. Each change has a weight: the
//! larger share of the statement's time, pages or cost its nodes take.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::compare::{self, Change, Comparison};
use crate::fingerprint;
use crate::format;
use crate::ir::{Node, NodeId, Plan, Relationship};
use crate::metrics::{self, Metrics};
use crate::rules::Evidence;

/// Changes smaller than this fraction are within the noise.
const NOISE: f64 = 0.1;
/// Time differences under this many milliseconds are within the noise.
const MIN_TIME: f64 = 0.1;
/// A matched node's changed work, a misestimate or a spill is listed when
/// it moves at least this share of the statement's time or pages.
const MIN_WEIGHT: f64 = 0.05;
/// Row estimates off by this factor or more are misestimates.
const MISESTIMATE: f64 = 10.0;

/// How two plans of the same statement differ.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanDiff {
    /// One sentence: how the second plan compares, and its main change.
    pub verdict: String,
    /// The statements' figures, pages first.
    pub comparison: Comparison,
    /// The plans' [shape ids](fingerprint::id).
    pub shapes: Shapes,
    /// What changed, the most significant first.
    pub changes: Vec<NodeChange>,
    /// Nodes that do the same work in both plans: (first plan, second plan),
    /// in the order of the second plan.
    pub matched: Vec<(NodeId, NodeId)>,
    /// Nodes of the first plan that the second plan does without.
    pub removed: Vec<NodeId>,
    /// Nodes of the second plan that the first plan did without.
    pub added: Vec<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shapes {
    pub before: String,
    pub after: String,
}

impl Shapes {
    pub fn same(&self) -> bool {
        self.before == self.after
    }
}

/// One change between the plans.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeChange {
    pub kind: ChangeKind,
    /// The node in the first plan, if it has one there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<NodeId>,
    /// The node in the second plan, if it has one there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<NodeId>,
    /// What changed, as a clause: `Index Scan using orders_customer_id_idx
    /// on orders o became Seq Scan on orders o`.
    pub summary: String,
    pub evidence: Vec<Evidence>,
    /// The larger share of the statement's time, pages or estimated cost
    /// that the change's nodes take, from 0 to 1.
    pub weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A relation is read another way: another scan type or index.
    Access,
    /// The same relations are joined by another method, or the sides of the
    /// join swapped.
    Join,
    /// The relations are joined in another order.
    JoinOrder,
    /// Another variant of the same operation: an aggregate's strategy, a
    /// sort that became incremental, a gather that keeps the order.
    Strategy,
    /// A node only the second plan has.
    Added,
    /// A node only the first plan has.
    Removed,
    /// A node that started or stopped writing temporary files.
    Spill,
    /// A row estimate that became 10× off or more, or stopped being.
    Estimate,
    /// The same node, whose time or pages changed by more than the noise.
    Work,
}

impl ChangeKind {
    /// The label of the change in reports.
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Access => "ACCESS",
            ChangeKind::Join => "JOIN",
            ChangeKind::JoinOrder => "ORDER",
            ChangeKind::Strategy => "STRATEGY",
            ChangeKind::Added => "ADDED",
            ChangeKind::Removed => "REMOVED",
            ChangeKind::Spill => "SPILL",
            ChangeKind::Estimate => "ESTIMATE",
            ChangeKind::Work => "WORK",
        }
    }

    /// Whether the plan's structure changed, rather than a node's numbers.
    pub fn structural(self) -> bool {
        !matches!(self, ChangeKind::Estimate | ChangeKind::Work)
    }
}

/// Compares two plans of the same statement.
pub fn diff(before: &Plan, after: &Plan) -> PlanDiff {
    let before = Side::new(before);
    let after = Side::new(after);
    let comparison = compare::between(
        compare::figures(before.plan, &before.metrics),
        compare::figures(after.plan, &after.metrics),
    );
    let shapes = Shapes {
        before: fingerprint::id(before.plan),
        after: fingerprint::id(after.plan),
    };
    let matched = matching(&before, &after);
    let in_before: BTreeSet<NodeId> = matched.iter().map(|&(id, _)| id).collect();
    let in_after: BTreeSet<NodeId> = matched.iter().map(|&(_, id)| id).collect();
    let removed: Vec<NodeId> = before
        .plan
        .walk()
        .into_iter()
        .map(|(_, node)| node.id)
        .filter(|id| !in_before.contains(id))
        .collect();
    let added: Vec<NodeId> = after
        .plan
        .walk()
        .into_iter()
        .map(|(_, node)| node.id)
        .filter(|id| !in_after.contains(id))
        .collect();

    let context = Context {
        before: &before,
        after: &after,
        matched: &matched,
    };
    let mut changes = Vec::new();
    for &(old, new) in &matched {
        changes.extend(context.pair_changes(old, new));
    }
    // Joins without a match on both sides are joins in another order; on
    // one side only, joins that came or went.
    let reordered = context.join_order(&removed, &added);
    let joins_told = reordered.is_some();
    changes.extend(reordered);
    changes.extend(context.unmatched(&removed, false, joins_told));
    changes.extend(context.unmatched(&added, true, joins_told));
    let mut changes = context.group_partitions(changes);
    changes.sort_by(|a, b| {
        b.kind
            .structural()
            .cmp(&a.kind.structural())
            .then(b.weight.total_cmp(&a.weight))
    });

    let verdict = verdict(&comparison, &shapes, &changes);
    PlanDiff {
        verdict,
        comparison,
        shapes,
        changes,
        matched,
        removed,
        added,
    }
}

/// A plan and its metrics.
struct Side<'a> {
    plan: &'a Plan,
    metrics: Metrics,
    /// Whether the plan was measured, with times.
    measured: bool,
    /// Pages read by the whole statement, when reported.
    pages: Option<u64>,
}

impl<'a> Side<'a> {
    fn new(plan: &'a Plan) -> Self {
        let metrics = metrics::compute(plan);
        let measured = metrics.statement.total_time.is_some()
            && plan
                .root()
                .actuals
                .is_some_and(|actuals| !actuals.never_executed());
        let pages = plan.root().buffers.map(|buffers| metrics::blocks(&buffers));
        Side {
            plan,
            metrics,
            measured,
            pages,
        }
    }

    fn node(&self, id: NodeId) -> &Node {
        self.plan.node(id)
    }

    fn time(&self, id: NodeId) -> Option<f64> {
        self.measured
            .then(|| self.metrics.node(id).exclusive_time)
            .flatten()
    }

    fn own_pages(&self, id: NodeId) -> Option<u64> {
        self.metrics
            .node(id)
            .exclusive_buffers
            .map(|buffers| metrics::blocks(&buffers))
    }

    fn temp_pages(&self, id: NodeId) -> Option<u64> {
        self.metrics
            .node(id)
            .exclusive_buffers
            .map(|buffers| buffers.temp_read + buffers.temp_written)
    }

    fn rows(&self, id: NodeId) -> Option<f64> {
        let actuals = self.node(id).actuals?;
        Some(actuals.rows * actuals.loops as f64)
    }

    /// The share of the statement a node takes: of its time and pages when
    /// measured, of its estimated cost (with its children) otherwise.
    fn weight(&self, id: NodeId) -> f64 {
        let metrics = self.metrics.node(id);
        let time = metrics.time_share.filter(|_| self.measured).unwrap_or(0.0);
        let pages = match (self.own_pages(id), self.pages) {
            (Some(own), Some(total)) if total > 0 => own as f64 / total as f64,
            _ => 0.0,
        };
        let cost = if self.measured {
            0.0
        } else {
            match (self.node(id).estimates, self.plan.root().estimates) {
                (Some(node), Some(root)) if root.total_cost > 0.0 => {
                    (node.total_cost / root.total_cost).min(1.0)
                }
                _ => 0.0,
            }
        };
        time.max(pages).max(cost)
    }
}

/// What a node does, to find it again in another plan. The first field is
/// the node's [scope](scope), when the pass uses it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    /// A scan, by what it reads.
    Scan(String, String),
    /// A join, by the relations below it.
    Join(String, BTreeSet<String>),
    /// Any other node, by its kind of operation and the relations below it.
    Other(String, String, BTreeSet<String>),
}

/// Nodes that do the same work in both plans, in the order of the second.
fn matching(before: &Side, after: &Side) -> Vec<(NodeId, NodeId)> {
    let mut pairs = Vec::new();
    let mut taken_before = BTreeSet::new();
    let mut taken_after = BTreeSet::new();
    // First by the full key within the node's part of the statement; then
    // scans by their relation alone, as partitions are renamed from one
    // version to the next; then anything wherever it is in the statement.
    let passes: [fn(&Plan, &Node) -> Option<Key>; 3] = [key, scan_by_relation, |plan, node| {
        unscoped(key(plan, node)?)
    }];
    for pass in passes {
        let mut slots: BTreeMap<Key, VecDeque<NodeId>> = BTreeMap::new();
        for (_, node) in before.plan.walk() {
            if taken_before.contains(&node.id) {
                continue;
            }
            if let Some(key) = pass(before.plan, node) {
                slots.entry(key).or_default().push_back(node.id);
            }
        }
        for (_, node) in after.plan.walk() {
            if taken_after.contains(&node.id) {
                continue;
            }
            let Some(key) = pass(after.plan, node) else {
                continue;
            };
            if let Some(id) = slots.get_mut(&key).and_then(VecDeque::pop_front) {
                taken_before.insert(id);
                taken_after.insert(node.id);
                pairs.push((id, node.id));
            }
        }
    }
    let order: BTreeMap<NodeId, usize> = after
        .plan
        .walk()
        .into_iter()
        .enumerate()
        .map(|(position, (_, node))| (node.id, position))
        .collect();
    pairs.sort_by_key(|&(_, id)| order[&id]);
    pairs
}

fn key(plan: &Plan, node: &Node) -> Option<Key> {
    let scope = scope(plan, node);
    if is_scan(node) {
        if let Some(reads) = reads(node) {
            return Some(Key::Scan(scope, reads));
        }
    }
    let below = below(plan, node.id);
    if fingerprint::is_join(node) {
        return Some(Key::Join(scope, below));
    }
    Some(Key::Other(scope, family(node), below))
}

fn scan_by_relation(plan: &Plan, node: &Node) -> Option<Key> {
    let relation = node.relation_name.as_ref().filter(|_| is_scan(node))?;
    Some(Key::Scan(scope(plan, node), relation.clone()))
}

/// The same key, for a node anywhere in the statement.
fn unscoped(key: Key) -> Option<Key> {
    Some(match key {
        Key::Scan(_, reads) => Key::Scan(String::new(), reads),
        Key::Join(_, below) => Key::Join(String::new(), below),
        Key::Other(_, family, below) => Key::Other(String::new(), family, below),
    })
}

/// The part of the statement a node belongs to: the main query, or the
/// InitPlan, SubPlan or CTE it is in. InitPlans and SubPlans go without
/// their numbers, which change from one version to the next.
fn scope(plan: &Plan, node: &Node) -> String {
    let mut current = node;
    loop {
        match current.relationship {
            Some(Relationship::InitPlan) => {
                return match current.subplan_name.as_deref() {
                    Some(name) if name.starts_with("CTE ") => name.to_owned(),
                    _ => "InitPlan".to_owned(),
                };
            }
            Some(Relationship::SubPlan) => return "SubPlan".to_owned(),
            _ => {}
        }
        match current.parent {
            Some(parent) => current = plan.node(parent),
            None => return String::new(),
        }
    }
}

/// Whether a node reads rows from somewhere: a table, an index, a CTE, a
/// function, a subquery. Bitmap index scans only build a bitmap for the
/// heap scan above them.
fn is_scan(node: &Node) -> bool {
    node.node_type.ends_with("Scan") && node.node_type != "Bitmap Index Scan"
}

/// What a scan reads, with its alias: `orders o`, `CTE totals t`.
fn reads(node: &Node) -> Option<String> {
    let alias = node.alias.as_deref();
    let object = node
        .relation_name
        .as_deref()
        .map(|name| (String::new(), name))
        .or_else(|| {
            node.cte_name
                .as_deref()
                .map(|name| ("CTE ".to_owned(), name))
        })
        .or_else(|| {
            node.function_name
                .as_deref()
                .map(|name| ("function ".to_owned(), name))
        });
    match (object, alias) {
        (Some((kind, name)), Some(alias)) if alias != name => Some(format!("{kind}{name} {alias}")),
        (Some((kind, name)), _) => Some(format!("{kind}{name}")),
        (None, Some(alias)) => Some(format!("{} {alias}", node.node_type)),
        (None, None) => None,
    }
}

/// The relations read at or below a node, without aliases and with numbers
/// blanked out, leaving out InitPlans and SubPlans, which are planned on
/// their own.
fn below(plan: &Plan, id: NodeId) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        let node = plan.node(id);
        if is_scan(node) {
            let name = node
                .relation_name
                .as_deref()
                .or(node.cte_name.as_deref())
                .or(node.function_name.as_deref())
                .or(node.alias.as_deref());
            if let Some(name) = name {
                found.insert(fingerprint::blank_numbers(name));
            }
        }
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

/// Node types that do the same thing in another way, under one name.
fn family(node: &Node) -> String {
    match node.node_type.as_str() {
        "Gather" | "Gather Merge" => "Gather".to_owned(),
        "Sort" | "Incremental Sort" => "Sort".to_owned(),
        "Append" | "Merge Append" => "Append".to_owned(),
        "Aggregate" | "Group" => "Aggregate".to_owned(),
        "Bitmap Index Scan" => format!(
            "Bitmap Index Scan {}",
            node.index_name.as_deref().unwrap_or_default()
        ),
        "ModifyTable" => format!(
            "ModifyTable {}",
            node.relation_name.as_deref().unwrap_or_default()
        ),
        other => other.to_owned(),
    }
}

/// Parts of an access path: they build bitmaps for a Bitmap Heap Scan,
/// whose change says what they became.
fn is_bitmap_part(node: &Node) -> bool {
    matches!(
        node.node_type.as_str(),
        "Bitmap Index Scan" | "BitmapAnd" | "BitmapOr"
    )
}

struct Context<'a> {
    before: &'a Side<'a>,
    after: &'a Side<'a>,
    matched: &'a [(NodeId, NodeId)],
}

impl Context<'_> {
    fn weight(&self, old: Option<NodeId>, new: Option<NodeId>) -> f64 {
        let before = old.map_or(0.0, |id| self.before.weight(id));
        let after = new.map_or(0.0, |id| self.after.weight(id));
        before.max(after)
    }

    /// The changes between two nodes that do the same work.
    fn pair_changes(&self, old: NodeId, new: NodeId) -> Vec<NodeChange> {
        let (a, b) = (self.before.node(old), self.after.node(new));
        let mut changes = Vec::new();
        let weight = self.weight(Some(old), Some(new));
        let structural = if is_scan(a) && is_scan(b) {
            self.access(old, new)
        } else if fingerprint::is_join(a) && fingerprint::is_join(b) {
            self.join(old, new)
        } else {
            self.strategy(old, new)
        };
        let spill = self.spill(old, new);
        if let Some((kind, summary, mut evidence)) = structural {
            evidence.extend(self.work_evidence(old, new));
            changes.push(NodeChange {
                kind,
                before: Some(old),
                after: Some(new),
                summary,
                evidence,
                weight,
            });
            changes.extend(spill);
        } else if let Some(mut spill) = spill {
            spill.evidence.extend(self.work_evidence(old, new));
            changes.push(spill);
        } else if let Some(change) = self.work(old, new) {
            changes.push(change);
        }
        changes.extend(self.estimate(old, new));
        changes
    }

    /// Another scan type, index or parallelism for the same relation.
    fn access(&self, old: NodeId, new: NodeId) -> Option<(ChangeKind, String, Vec<Evidence>)> {
        let (a, b) = (self.before.node(old), self.after.node(new));
        // Bitmap heap scans read through the indexes of the bitmaps below
        // them; other scans name their index.
        let bitmaps = |side: &Side, node: &Node| {
            if node.node_type == "Bitmap Heap Scan" {
                fingerprint::indexes_with_condition(side.plan, node)
            } else {
                Vec::new()
            }
        };
        let (indexes_a, indexes_b) = (bitmaps(self.before, a), bitmaps(self.after, b));
        if a.node_type == b.node_type
            && a.index_name == b.index_name
            && a.parallel_aware == b.parallel_aware
            && backward(a) == backward(b)
            && indexes_a == indexes_b
        {
            return None;
        }
        let mut evidence = Vec::new();
        for (label, node) in [("Before", a), ("After", b)] {
            let conditions: Vec<String> = node
                .predicates
                .iter()
                .map(|predicate| format!("{} {}", predicate.kind.key(), predicate.text))
                .collect();
            if !conditions.is_empty() {
                evidence.push(Evidence {
                    label,
                    value: conditions.join(", "),
                });
            }
        }
        Some((
            ChangeKind::Access,
            format!(
                "{} became {}",
                access_label(self.before.plan, a),
                access_label(self.after.plan, b)
            ),
            evidence,
        ))
    }

    /// Another join method for the same relations, or the sides swapped.
    fn join(&self, old: NodeId, new: NodeId) -> Option<(ChangeKind, String, Vec<Evidence>)> {
        let (a, b) = (self.before.node(old), self.after.node(new));
        let relations = relation_list(self.after.plan, new);
        let outer_a = outer_side(self.before.plan, old);
        let outer_b = outer_side(self.after.plan, new);
        if a.node_type != b.node_type || a.join_type != b.join_type {
            return Some((
                ChangeKind::Join,
                format!(
                    "{} of {relations} became {}",
                    format::node(a),
                    format::node(b)
                ),
                Vec::new(),
            ));
        }
        if outer_a == outer_b || outer_b.is_empty() {
            return None;
        }
        let side = list(&outer_b);
        let summary = match b.node_type.as_str() {
            "Hash Join" => format!(
                "{} of {relations} now probes the hash table with {side}",
                format::node(b)
            ),
            "Nested Loop" => format!("{} of {relations} now loops over {side}", format::node(b)),
            _ => format!(
                "{} of {relations} now has {side} on its outer side",
                format::node(b)
            ),
        };
        Some((ChangeKind::Join, summary, Vec::new()))
    }

    /// Another variant of the same operation.
    fn strategy(&self, old: NodeId, new: NodeId) -> Option<(ChangeKind, String, Vec<Evidence>)> {
        let (a, b) = (self.before.node(old), self.after.node(new));
        if operation(a) == operation(b) {
            return None;
        }
        let (label_a, label_b) = (format::node(a), format::node(b));
        let mut evidence = Vec::new();
        let relations = relation_list(self.after.plan, new);
        if !relations.is_empty() {
            evidence.push(Evidence {
                label: "Over",
                value: relations,
            });
        }
        Some((
            ChangeKind::Strategy,
            format!("{label_a} became {label_b}"),
            evidence,
        ))
    }

    /// The same node doing more or less work, beyond the noise.
    fn work(&self, old: NodeId, new: NodeId) -> Option<NodeChange> {
        if !(self.before.measured && self.after.measured) {
            return None;
        }
        let time = self.before.time(old).zip(self.after.time(new));
        let pages = self.before.own_pages(old).zip(self.after.own_pages(new));
        let time_moved = time.filter(|&(a, b)| (a - b).abs() >= MIN_TIME && apart(a, b));
        let pages_moved = pages.filter(|&(a, b)| apart(a as f64, b as f64));
        let total_time = self
            .before
            .metrics
            .statement
            .total_time
            .zip(self.after.metrics.statement.total_time)
            .map(|(a, b)| a.max(b));
        let total_pages = self
            .before
            .pages
            .zip(self.after.pages)
            .map(|(a, b)| a.max(b));
        let moved = time_moved
            .zip(total_time)
            .map_or(0.0, |((a, b), total)| {
                (a - b).abs() / total.max(f64::MIN_POSITIVE)
            })
            .max(pages_moved.zip(total_pages).map_or(0.0, |((a, b), total)| {
                a.abs_diff(b) as f64 / total.max(1) as f64
            }));
        if moved < MIN_WEIGHT {
            return None;
        }
        let mut parts = Vec::new();
        if let Some((a, b)) = pages_moved {
            parts.push(format!(
                "pages {} → {}{}",
                count(a),
                count(b),
                ratio(a as f64, b as f64, "fewer", "more")
            ));
        }
        if let Some((a, b)) = time_moved {
            parts.push(format!(
                "time {} → {}{}",
                format::duration(a),
                format::duration(b),
                ratio(a, b, "faster", "slower")
            ));
            // Pages first: the same pages in another time may be the cache
            // or the load of the server rather than the plan.
            if pages_moved.is_none() {
                let reads = |side: &Side, id: NodeId| {
                    side.metrics
                        .node(id)
                        .exclusive_buffers
                        .map(|buffers| buffers.shared_read + buffers.local_read)
                };
                let last = parts.pop().unwrap_or_default();
                parts.push(match reads(self.before, old).zip(reads(self.after, new)) {
                    Some((a, b)) if a != b => format!(
                        "{last} for the same pages, {} → {} of them read from disk",
                        count(a),
                        count(b)
                    ),
                    _ => format!("{last} for the same pages and disk reads"),
                });
            }
        }
        Some(NodeChange {
            kind: ChangeKind::Work,
            before: Some(old),
            after: Some(new),
            summary: format!(
                "{}: {}",
                format::node(self.after.node(new)),
                parts.join(", ")
            ),
            evidence: self.work_evidence(old, new),
            weight: moved,
        })
    }

    /// Pages, time, rows and loops before and after, where they changed.
    fn work_evidence(&self, old: NodeId, new: NodeId) -> Vec<Evidence> {
        let mut evidence = Vec::new();
        if let Some((a, b)) = self.before.own_pages(old).zip(self.after.own_pages(new)) {
            if a > 0 || b > 0 {
                evidence.push(Evidence {
                    label: "Pages",
                    value: format!("{} → {}", count(a), count(b)),
                });
            }
        }
        if let Some((a, b)) = self.before.time(old).zip(self.after.time(new)) {
            if a > 0.0 || b > 0.0 {
                evidence.push(Evidence {
                    label: "Time",
                    value: format!("{} → {}", format::duration(a), format::duration(b)),
                });
            }
        }
        if let Some((a, b)) = self.before.rows(old).zip(self.after.rows(new)) {
            if a != b {
                evidence.push(Evidence {
                    label: "Rows",
                    value: format!("{} → {}", format::rows(a), format::rows(b)),
                });
            }
        }
        let loops = |side: &Side, id: NodeId| side.node(id).actuals.map(|actuals| actuals.loops);
        if let Some((a, b)) = loops(self.before, old).zip(loops(self.after, new)) {
            if a != b {
                evidence.push(Evidence {
                    label: "Loops",
                    value: format!("{} → {}", format::rows(a as f64), format::rows(b as f64)),
                });
            }
        }
        if let (Some(a), Some(b)) = (
            self.before.node(old).estimates,
            self.after.node(new).estimates,
        ) {
            if !self.before.measured || !self.after.measured {
                evidence.push(Evidence {
                    label: "Estimated rows",
                    value: format!("{} → {}", format::rows(a.rows), format::rows(b.rows)),
                });
            }
        }
        evidence
    }

    /// A node that started or stopped writing temporary files.
    fn spill(&self, old: NodeId, new: NodeId) -> Option<NodeChange> {
        let (a, b) = self
            .before
            .temp_pages(old)
            .zip(self.after.temp_pages(new))?;
        let label = format::node(self.after.node(new));
        let summary = match (a, b) {
            (0, pages) if pages > 0 => format!(
                "{label} now spills to disk: {} of temporary files",
                format::kilobytes(pages as f64 * 8.0)
            ),
            (pages, 0) if pages > 0 => format!("{label} no longer spills to disk"),
            _ => return None,
        };
        let weight = self.weight(Some(old), Some(new));
        Some(NodeChange {
            kind: ChangeKind::Spill,
            before: Some(old),
            after: Some(new),
            summary,
            evidence: vec![Evidence {
                label: "Temporary pages",
                value: format!("{} → {}", count(a), count(b)),
            }],
            weight,
        })
    }

    /// A row estimate that became far off, or stopped being.
    fn estimate(&self, old: NodeId, new: NodeId) -> Option<NodeChange> {
        let weight = self.weight(Some(old), Some(new));
        if weight < MIN_WEIGHT {
            return None;
        }
        let off = |side: &Side, id: NodeId| {
            side.metrics
                .node(id)
                .misestimate
                .filter(|error| error.factor >= MISESTIMATE)
        };
        // A misestimate carries up the tree: report it where it starts.
        let inherited = |side: &Side, id: NodeId| {
            side.plan
                .children(id)
                .any(|child| off(side, child.id).is_some())
        };
        let node = self.after.node(new);
        let label = format::node(node);
        let summary = match (off(self.before, old), off(self.after, new)) {
            (None, Some(error)) if !inherited(self.after, new) => {
                let expected = node.estimates.map_or(0.0, |estimates| estimates.rows);
                let actual = node.actuals.map_or(0.0, |actuals| actuals.rows);
                format!(
                    "The planner now expects {} from {label} and gets {} ({} {})",
                    rows_of(expected),
                    format::rows(actual),
                    format::factor(error.factor),
                    if error.underestimated {
                        "more"
                    } else {
                        "fewer"
                    }
                )
            }
            (Some(_), None) if !inherited(self.before, old) => {
                format!("The row estimate of {label} is no longer far off")
            }
            _ => return None,
        };
        Some(NodeChange {
            kind: ChangeKind::Estimate,
            before: Some(old),
            after: Some(new),
            summary,
            evidence: Vec::new(),
            weight,
        })
    }

    /// The same access change on several partitions of a table, as one.
    fn group_partitions(&self, changes: Vec<NodeChange>) -> Vec<NodeChange> {
        let group = |change: &NodeChange| {
            let (old, new) = (change.before?, change.after?);
            let (a, b) = (self.before.node(old), self.after.node(new));
            let relation = a.relation_name.as_deref()?;
            let pattern = fingerprint::blank_numbers(relation);
            let index = |node: &Node| {
                node.index_name
                    .as_deref()
                    .map(fingerprint::blank_numbers)
                    .unwrap_or_default()
            };
            (change.kind == ChangeKind::Access && pattern != relation).then(|| {
                (
                    pattern,
                    format!("{}|{}", access_type(a), index(a)),
                    format!("{}|{}", access_type(b), index(b)),
                )
            })
        };
        let mut groups: BTreeMap<(String, String, String), Vec<usize>> = BTreeMap::new();
        for (index, change) in changes.iter().enumerate() {
            if let Some(key) = group(change) {
                groups.entry(key).or_default().push(index);
            }
        }
        let mut merged: BTreeMap<usize, NodeChange> = BTreeMap::new();
        let mut absorbed = BTreeSet::new();
        for ((_, from, to), indexes) in groups.into_iter().filter(|(_, indexes)| indexes.len() > 1)
        {
            let (from, to) = (
                from.split('|').next().unwrap_or_default(),
                to.split('|').next().unwrap_or_default(),
            );
            let names: Vec<&str> = indexes
                .iter()
                .filter_map(|&index| changes[index].before)
                .filter_map(|id| self.before.node(id).relation_name.as_deref())
                .collect();
            let mut shown: Vec<String> = names
                .iter()
                .take(6)
                .map(|name| (*name).to_owned())
                .collect();
            if names.len() > 6 {
                shown.push(format!("{} more", names.len() - 6));
            }
            let first = &changes[indexes[0]];
            let mut evidence = vec![Evidence {
                label: "Partitions",
                value: shown.join(", "),
            }];
            evidence.extend(
                first
                    .evidence
                    .iter()
                    .filter(|evidence| matches!(evidence.label, "Before" | "After"))
                    .cloned(),
            );
            merged.insert(
                indexes[0],
                NodeChange {
                    kind: ChangeKind::Access,
                    before: first.before,
                    after: first.after,
                    summary: format!(
                        "{from} became {to} on {} partitions of {}",
                        indexes.len(),
                        common_name(names.into_iter())
                    ),
                    evidence,
                    weight: indexes
                        .iter()
                        .map(|&index| changes[index].weight)
                        .sum::<f64>()
                        .min(1.0),
                },
            );
            absorbed.extend(indexes[1..].iter().copied());
        }
        changes
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !absorbed.contains(index))
            .map(|(index, change)| merged.remove(&index).unwrap_or(change))
            .collect()
    }

    /// Joins without a match on both sides: the relations are joined in
    /// another order.
    fn join_order(&self, removed: &[NodeId], added: &[NodeId]) -> Option<NodeChange> {
        let first = |side: &Side, ids: &[NodeId]| {
            ids.iter()
                .copied()
                .filter(|&id| fingerprint::is_join(side.node(id)))
                .min_by_key(|&id| (below(side.plan, id).len(), id))
        };
        let old = first(self.before, removed)?;
        let new = first(self.after, added)?;
        let weight = self.weight(Some(old), Some(new));
        Some(NodeChange {
            kind: ChangeKind::JoinOrder,
            before: Some(old),
            after: Some(new),
            summary: format!(
                "The relations are joined in another order: first {}, not {}",
                relation_list(self.after.plan, new),
                relation_list(self.before.plan, old)
            ),
            evidence: Vec::new(),
            weight,
        })
    }

    /// Nodes without a match in the other plan, except those a change of
    /// their parent explains: bitmap index scans, the Hash of a hash join,
    /// and joins when the join order says what became of them. Partitions
    /// read or no longer read are counted together.
    fn unmatched(&self, ids: &[NodeId], added: bool, joins_told: bool) -> Vec<NodeChange> {
        let side = if added { self.after } else { self.before };
        let matched_parent = |node: &Node| {
            node.parent.is_some_and(|parent| {
                self.matched.iter().any(
                    |&(old, new)| {
                        if added { new == parent } else { old == parent }
                    },
                )
            })
        };
        let mut changes = Vec::new();
        let mut partitions: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for &id in ids {
            let node = side.node(id);
            if is_bitmap_part(node)
                || (joins_told && fingerprint::is_join(node))
                || (node.node_type == "Hash" && matched_parent(node))
            {
                continue;
            }
            if is_scan(node) {
                if let Some(relation) = &node.relation_name {
                    let pattern = fingerprint::blank_numbers(relation);
                    if pattern != *relation {
                        partitions.entry(pattern).or_default().push(id);
                        continue;
                    }
                }
            }
            let label = format::node(node);
            let mut evidence = Vec::new();
            let mut summary = if added {
                format!("{label} added")
            } else {
                format!("{label} removed")
            };
            if node.node_type == "Gather" || node.node_type == "Gather Merge" {
                summary = if added {
                    let workers = node
                        .workers_planned
                        .map(|workers| format!(" with {workers} workers planned"))
                        .unwrap_or_default();
                    format!(
                        "{label} added: the part of the plan below it runs in parallel{workers}"
                    )
                } else {
                    format!("{label} removed: no part of the plan runs in parallel any more")
                };
            }
            if let Some(child) = side.plan.children(id).next() {
                evidence.push(Evidence {
                    label: if added { "Above" } else { "Was above" },
                    value: format::node(child),
                });
            }
            changes.push(NodeChange {
                kind: if added {
                    ChangeKind::Added
                } else {
                    ChangeKind::Removed
                },
                before: (!added).then_some(id),
                after: added.then_some(id),
                summary,
                evidence,
                weight: side.weight(id),
            });
        }
        for (pattern, ids) in partitions {
            let total = |side: &Side| {
                side.plan
                    .nodes
                    .iter()
                    .filter(|node| {
                        is_scan(node)
                            && node
                                .relation_name
                                .as_deref()
                                .is_some_and(|name| fingerprint::blank_numbers(name) == pattern)
                    })
                    .count()
            };
            let name = common_name(
                ids.iter()
                    .filter_map(|&id| side.node(id).relation_name.as_deref()),
            );
            let (before, after) = (total(self.before), total(self.after));
            let summary = if ids.len() == 1 {
                let label = format::node(side.node(ids[0]));
                if added {
                    format!("{label} added")
                } else {
                    format!("{label} removed")
                }
            } else if added {
                format!(
                    "{} more partitions of {name} read: {after}, not {before}",
                    ids.len()
                )
            } else {
                format!(
                    "{} fewer partitions of {name} read: {after}, not {before}",
                    ids.len()
                )
            };
            let weight = ids.iter().map(|&id| side.weight(id)).sum::<f64>().min(1.0);
            changes.push(NodeChange {
                kind: if added {
                    ChangeKind::Added
                } else {
                    ChangeKind::Removed
                },
                before: (!added).then_some(ids[0]),
                after: added.then_some(ids[0]),
                summary,
                evidence: Vec::new(),
                weight,
            });
        }
        changes
    }
}

/// What a node does and how, apart from what it reads and its name in the
/// plan: two nodes with the same operation do the same thing the same way.
#[derive(PartialEq, Eq)]
struct Operation<'a> {
    node_type: &'a str,
    strategy: Option<&'a str>,
    partial_mode: Option<&'a str>,
    join_type: Option<&'a str>,
    parallel: bool,
    command: Option<&'a str>,
    operation: Option<&'a str>,
}

fn operation(node: &Node) -> Operation<'_> {
    Operation {
        node_type: node.node_type.as_str(),
        strategy: node.strategy.as_deref(),
        partial_mode: node.partial_mode.as_deref(),
        join_type: node.join_type.as_deref(),
        parallel: node.parallel_aware,
        command: node.command.as_deref(),
        operation: node.operation.as_deref(),
    }
}

/// How a scan reads, without what or through which index: `Index Scan
/// Backward`, `Bitmap Heap Scan`, `Parallel Seq Scan`.
fn access_type(node: &Node) -> String {
    let mut kind = String::new();
    if node.parallel_aware {
        kind.push_str("Parallel ");
    }
    kind.push_str(&node.node_type);
    if backward(node) {
        kind.push_str(" Backward");
    }
    kind
}

/// Whether an index scan reads its index backward, for a descending order.
fn backward(node: &Node) -> bool {
    node.scan_direction.as_deref() == Some("Backward")
}

/// A scan's label, with its direction when backward, and the indexes a
/// Bitmap Heap Scan reads through.
fn access_label(plan: &Plan, node: &Node) -> String {
    let mut label = format::node(node);
    if backward(node) {
        label = label.replacen(&node.node_type, &format!("{} Backward", node.node_type), 1);
    }
    if node.node_type != "Bitmap Heap Scan" {
        return label;
    }
    let indexes = fingerprint::indexes_with_condition(plan, node);
    if indexes.is_empty() {
        label
    } else {
        format!("{label} (through {})", indexes.join(", "))
    }
}

/// The relations on the outer side of a join.
fn outer_side(plan: &Plan, id: NodeId) -> BTreeSet<String> {
    plan.children(id)
        .find(|child| child.relationship == Some(Relationship::Outer))
        .map(|child| scans_below(plan, child.id))
        .unwrap_or_default()
}

/// What the scans at or below a node read, with aliases, for people to
/// read: `orders o`, `totals t`.
fn scans_below(plan: &Plan, id: NodeId) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        let node = plan.node(id);
        if is_scan(node) {
            let object = node
                .relation_name
                .as_deref()
                .or(node.cte_name.as_deref())
                .or(node.function_name.as_deref());
            found.extend(match (object, node.alias.as_deref()) {
                (Some(object), Some(alias)) if alias != object => Some(format!("{object} {alias}")),
                (Some(object), _) => Some(object.to_owned()),
                (None, alias) => alias.map(str::to_owned),
            });
        }
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

/// The relations below a node, in words: `customers c and orders o`.
fn relation_list(plan: &Plan, id: NodeId) -> String {
    list(&scans_below(plan, id))
}

fn list(items: &BTreeSet<String>) -> String {
    let items: Vec<&str> = items.iter().map(String::as_str).collect();
    match items.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What partition names have in common: `events_2025_*`.
fn common_name<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = first.len();
    for name in &names[1..] {
        prefix = first
            .bytes()
            .zip(name.bytes())
            .take(prefix)
            .take_while(|(a, b)| a == b)
            .count();
    }
    // Back to a character boundary.
    while !first.is_char_boundary(prefix) {
        prefix -= 1;
    }
    if prefix == first.len() {
        (*first).to_owned()
    } else {
        format!("{}*", &first[..prefix])
    }
}

/// More than [`NOISE`] apart, either way.
fn apart(a: f64, b: f64) -> bool {
    b > a * (1.0 + NOISE) || b < a / (1.0 + NOISE)
}

fn count(value: u64) -> String {
    format::grouped(i64::try_from(value).unwrap_or(i64::MAX))
}

/// ` (3.2× more)`, or nothing when one side is zero or they are within the
/// noise.
fn ratio(before: f64, after: f64, less: &str, more: &str) -> String {
    if before <= 0.0 || after <= 0.0 || !apart(before, after) {
        return String::new();
    }
    if after < before {
        format!(" ({} {less})", format::factor(before / after))
    } else {
        format!(" ({} {more})", format::factor(after / before))
    }
}

/// `1 row`, `50,000 rows`.
fn rows_of(count: f64) -> String {
    let rows = format::rows(count);
    if rows == "1" {
        "1 row".to_owned()
    } else {
        format!("{rows} rows")
    }
}

/// How the second plan compares, then its main change.
fn verdict(comparison: &Comparison, shapes: &Shapes, changes: &[NodeChange]) -> String {
    let details = comparison.details();
    let mut sentence = match comparison.change {
        Change::Unknown => "The plans have no figures to compare.".to_owned(),
        _ if details.is_empty() => format!("{}.", capitalize(comparison.describe())),
        _ => format!("{}: {details}.", capitalize(comparison.describe())),
    };
    let main = changes.iter().find(|change| change.kind.structural());
    match main {
        _ if shapes.same() => match changes.first() {
            Some(change) => sentence.push_str(&format!(
                " The plan is the same; {}.",
                lowercase_first(&change.summary)
            )),
            None => sentence.push_str(" The plan is the same."),
        },
        Some(change) => {
            sentence.push_str(&format!(" {}.", capitalize(&change.summary)));
            let others = changes
                .iter()
                .filter(|change| change.kind.structural())
                .count()
                - 1;
            if others > 0 {
                sentence.push_str(&format!(
                    " {} other change{} in the plan.",
                    others,
                    if others == 1 { "" } else { "s" }
                ));
            }
        }
        None => sentence.push_str(" The plans differ in details of their nodes only."),
    }
    sentence
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        // Node names keep their capitals: `Seq Scan on orders: …`.
        Some(first) if text.starts_with("The ") => first.to_lowercase().chain(chars).collect(),
        Some(_) => text.to_owned(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Plan {
        let plan = crate::parse(text).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        plan
    }

    const INDEXED: &str = "\
Index Scan using orders_customer_id_idx on orders o  (cost=0.42..44.50 rows=10 width=20) (actual time=0.020..0.051 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=13
Planning Time: 0.100 ms
Execution Time: 0.070 ms";

    const SCANNED: &str = "\
Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Planning Time: 0.100 ms
Execution Time: 11.899 ms";

    #[test]
    fn the_same_plan_has_no_changes() {
        let plan = parse(SCANNED);
        let same = diff(&plan, &plan);
        assert!(same.shapes.same());
        assert!(same.changes.is_empty(), "{:?}", same.changes);
        assert_eq!(same.matched, [(NodeId(0), NodeId(0))]);
        assert_eq!(same.comparison.change, Change::Same);
        assert_eq!(
            same.verdict,
            "No different, within 10%: pages 2,417 → 2,417, execution 11.9 ms → 11.9 ms. The plan is the same."
        );
    }

    #[test]
    fn another_access_path() {
        let worse = diff(&parse(INDEXED), &parse(SCANNED));
        assert!(!worse.shapes.same());
        let change = &worse.changes[0];
        assert_eq!(change.kind, ChangeKind::Access);
        assert_eq!(
            change.summary,
            "Index Scan using orders_customer_id_idx on orders o became Seq Scan on orders o"
        );
        let evidence: Vec<String> = change
            .evidence
            .iter()
            .map(|evidence| format!("{}: {}", evidence.label, evidence.value))
            .collect();
        assert_eq!(
            evidence,
            [
                "Before: Index Cond (customer_id = 4242)",
                "After: Filter (customer_id = 4242)",
                "Pages: 13 → 2,417",
                "Time: 0.051 ms → 11.9 ms",
            ]
        );
        assert_eq!(worse.changes.len(), 1);
        assert_eq!(
            worse.verdict,
            "Worse: pages 13 → 2,417 (186× more), execution 0.070 ms → 11.9 ms (170× slower). Index Scan using orders_customer_id_idx on orders o became Seq Scan on orders o."
        );
        // And back.
        let better = diff(&parse(SCANNED), &parse(INDEXED));
        assert!(
            better
                .verdict
                .starts_with("Better: pages 2,417 → 13 (186× fewer)")
        );
    }

    const HASHED: &str = "\
Hash Join  (cost=10.00..2500.00 rows=100 width=8) (actual time=0.500..30.000 rows=100 loops=1)
  Hash Cond: (oi.order_id = o.id)
  Buffers: shared hit=1960
  ->  Seq Scan on order_items oi  (cost=0.00..1900.00 rows=300000 width=8) (actual time=0.010..20.000 rows=300000 loops=1)
        Buffers: shared hit=1911
  ->  Hash  (cost=9.00..9.00 rows=10 width=4) (actual time=0.300..0.300 rows=10 loops=1)
        Buckets: 1024  Batches: 1  Memory Usage: 9kB
        Buffers: shared hit=49
        ->  Index Scan using orders_customer_id_idx on orders o  (cost=0.42..9.00 rows=10 width=4) (actual time=0.020..0.200 rows=10 loops=1)
              Index Cond: (customer_id = 4242)
              Buffers: shared hit=49
Execution Time: 30.100 ms";

    const LOOPED: &str = "\
Nested Loop  (cost=0.85..90.00 rows=100 width=8) (actual time=0.030..0.400 rows=100 loops=1)
  Buffers: shared hit=89
  ->  Index Scan using orders_customer_id_idx on orders o  (cost=0.42..9.00 rows=10 width=4) (actual time=0.020..0.050 rows=10 loops=1)
        Index Cond: (customer_id = 4242)
        Buffers: shared hit=13
  ->  Index Scan using order_items_order_id_idx on order_items oi  (cost=0.42..8.00 rows=10 width=8) (actual time=0.005..0.030 rows=10 loops=10)
        Index Cond: (order_id = o.id)
        Buffers: shared hit=76
Execution Time: 0.450 ms";

    #[test]
    fn another_join_method() {
        let better = diff(&parse(HASHED), &parse(LOOPED));
        let summaries: Vec<(ChangeKind, &str)> = better
            .changes
            .iter()
            .map(|change| (change.kind, change.summary.as_str()))
            .collect();
        assert_eq!(
            summaries,
            [
                (
                    ChangeKind::Access,
                    "Seq Scan on order_items oi became Index Scan using order_items_order_id_idx on order_items oi"
                ),
                (
                    ChangeKind::Join,
                    "Hash Join of order_items oi and orders o became Nested Loop"
                ),
            ]
        );
        let evidence: Vec<String> = better.changes[1]
            .evidence
            .iter()
            .map(|evidence| format!("{}: {}", evidence.label, evidence.value))
            .collect();
        assert_eq!(evidence, ["Time: 9.70 ms → 0.050 ms"]);
        // The Hash under the hash join is part of the join's change.
        assert_eq!(better.removed, [NodeId(2)]);
        assert_eq!(
            better.verdict,
            "Better: pages 1,960 → 89 (22× fewer), execution 30.1 ms → 0.450 ms (67× faster). Seq Scan on order_items oi became Index Scan using order_items_order_id_idx on order_items oi. 1 other change in the plan."
        );
    }

    #[test]
    fn another_join_order() {
        let first = parse(
            "\
Hash Join  (cost=1.00..30.00 rows=10 width=8)
  Hash Cond: (oi.order_id = o.id)
  ->  Seq Scan on order_items oi  (cost=0.00..10.00 rows=100 width=8)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4)
        ->  Hash Join  (cost=0.50..1.00 rows=10 width=4)
              Hash Cond: (o.customer_id = c.id)
              ->  Seq Scan on orders o  (cost=0.00..0.40 rows=10 width=8)
              ->  Hash  (cost=0.20..0.20 rows=1 width=4)
                    ->  Seq Scan on customers c  (cost=0.00..0.20 rows=1 width=4)",
        );
        let second = parse(
            "\
Hash Join  (cost=1.00..20.00 rows=10 width=8)
  Hash Cond: (o.customer_id = c.id)
  ->  Hash Join  (cost=0.50..15.00 rows=100 width=12)
        Hash Cond: (oi.order_id = o.id)
        ->  Seq Scan on order_items oi  (cost=0.00..10.00 rows=100 width=8)
        ->  Hash  (cost=0.40..0.40 rows=10 width=8)
              ->  Seq Scan on orders o  (cost=0.00..0.40 rows=10 width=8)
  ->  Hash  (cost=0.20..0.20 rows=1 width=4)
        ->  Seq Scan on customers c  (cost=0.00..0.20 rows=1 width=4)",
        );
        let changed = diff(&first, &second);
        let order = changed
            .changes
            .iter()
            .find(|change| change.kind == ChangeKind::JoinOrder)
            .unwrap();
        assert_eq!(
            order.summary,
            "The relations are joined in another order: first order_items oi and orders o, not customers c and orders o"
        );
        // Every scan is matched, and the top join too.
        assert!(changed.matched.contains(&(NodeId(0), NodeId(0))));
        assert_eq!(changed.comparison.basis, Some(compare::Basis::Cost));
    }

    #[test]
    fn nodes_that_come_and_go() {
        let sorted = parse(
            "\
Sort  (cost=5000.00..5000.03 rows=10 width=20) (actual time=12.000..12.001 rows=10 loops=1)
  Sort Key: created_at
  Sort Method: quicksort  Memory: 25kB
  Buffers: shared hit=2417
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20) (actual time=1.053..11.865 rows=10 loops=1)
        Filter: (customer_id = 4242)
        Rows Removed by Filter: 199990
        Buffers: shared hit=2417
Execution Time: 12.100 ms",
        );
        let ordered = parse(
            "\
Index Scan using orders_customer_id_created_at_idx on orders o  (cost=0.42..44.50 rows=10 width=20) (actual time=0.020..0.051 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=4
Execution Time: 0.070 ms",
        );
        let changed = diff(&sorted, &ordered);
        let summaries: Vec<&str> = changed
            .changes
            .iter()
            .map(|change| change.summary.as_str())
            .collect();
        assert!(summaries.contains(&"Sort removed"), "{summaries:?}");
        assert!(summaries[0].starts_with("Seq Scan on orders o became Index Scan"));
    }

    #[test]
    fn parallelism_and_partitions() {
        let serial = parse(
            "\
Append  (cost=0.00..300.00 rows=120 width=8) (actual time=0.010..3.000 rows=120 loops=1)
  ->  Seq Scan on events_2025_01 events_1  (cost=0.00..100.00 rows=40 width=8) (actual time=0.010..1.000 rows=40 loops=1)
  ->  Seq Scan on events_2025_02 events_2  (cost=0.00..100.00 rows=40 width=8) (actual time=0.010..1.000 rows=40 loops=1)
  ->  Seq Scan on events_2025_03 events_3  (cost=0.00..100.00 rows=40 width=8) (actual time=0.010..1.000 rows=40 loops=1)
Execution Time: 3.100 ms",
        );
        let pruned = parse(
            "\
Gather  (cost=0.00..100.00 rows=40 width=8) (actual time=0.300..1.200 rows=40 loops=1)
  Workers Planned: 2
  Workers Launched: 2
  ->  Parallel Append  (cost=0.00..100.00 rows=40 width=8) (actual time=0.010..0.500 rows=13 loops=3)
        ->  Parallel Seq Scan on events_2025_03 events_1  (cost=0.00..100.00 rows=40 width=8) (actual time=0.010..0.400 rows=13 loops=3)
Execution Time: 1.300 ms",
        );
        let changed = diff(&serial, &pruned);
        let summaries: Vec<&str> = changed
            .changes
            .iter()
            .map(|change| change.summary.as_str())
            .collect();
        assert!(
            summaries.contains(
                &"Gather added: the part of the plan below it runs in parallel with 2 workers planned"
            ),
            "{summaries:?}"
        );
        assert!(
            summaries.contains(&"2 fewer partitions of events_2025_0* read: 1, not 3"),
            "{summaries:?}"
        );
        assert!(
            summaries.contains(
                &"Seq Scan on events_2025_03 events_3 became Parallel Seq Scan on events_2025_03 events_1"
            ),
            "{summaries:?}"
        );
        assert!(
            summaries.contains(&"Append became Parallel Append"),
            "{summaries:?}"
        );
    }

    #[test]
    fn misestimates_and_spills() {
        let fine = parse(
            "\
Sort  (cost=5000.00..5100.00 rows=50000 width=20) (actual time=20.000..25.000 rows=50000 loops=1)
  Sort Key: weight
  Sort Method: quicksort  Memory: 3000kB
  Buffers: shared hit=589
  ->  Seq Scan on shipments s  (cost=0.00..1089.00 rows=50000 width=20) (actual time=0.010..5.000 rows=50000 loops=1)
        Filter: (state = 'active'::text)
        Rows Removed by Filter: 50000
        Buffers: shared hit=589
Execution Time: 26.000 ms",
        );
        let off = parse(
            "\
Sort  (cost=5000.00..5000.01 rows=1 width=20) (actual time=60.000..70.000 rows=50000 loops=1)
  Sort Key: weight
  Sort Method: external merge  Disk: 1600kB
  Buffers: shared hit=589, temp read=200 written=200
  ->  Seq Scan on shipments s  (cost=0.00..1089.00 rows=1 width=20) (actual time=0.010..5.000 rows=50000 loops=1)
        Filter: (state = 'active'::text)
        Rows Removed by Filter: 50000
        Buffers: shared hit=589
Execution Time: 71.000 ms",
        );
        let changed = diff(&fine, &off);
        let summaries: Vec<&str> = changed
            .changes
            .iter()
            .map(|change| change.summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            [
                "Sort now spills to disk: 3.1 MB of temporary files",
                // Where the misestimate starts, not again in the Sort above.
                "The planner now expects 1 row from Seq Scan on shipments s and gets 50,000 (50,000× more)",
            ]
        );
        // The spill says what it cost.
        let evidence: Vec<String> = changed.changes[0]
            .evidence
            .iter()
            .map(|evidence| format!("{}: {}", evidence.label, evidence.value))
            .collect();
        assert_eq!(
            evidence,
            ["Temporary pages: 0 → 400", "Time: 20.0 ms → 65.0 ms"]
        );
        // And back.
        let fixed = diff(&off, &fine);
        assert_eq!(fixed.changes[0].summary, "Sort no longer spills to disk");
        assert_eq!(
            fixed.changes[1].summary,
            "The row estimate of Seq Scan on shipments s is no longer far off"
        );
    }

    #[test]
    fn joins_that_go_away() {
        // The planner removed a LEFT JOIN to a table that adds no rows.
        let joined = parse(
            "\
Hash Left Join  (cost=10.00..5000.00 rows=10 width=20)
  Hash Cond: (o.customer_id = c.id)
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20)
        Filter: (status = 'pending'::text)
  ->  Hash  (cost=5.00..5.00 rows=200 width=4)
        ->  Seq Scan on customers c  (cost=0.00..5.00 rows=200 width=4)",
        );
        let alone = parse(
            "Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20)\n  Filter: (status = 'pending'::text)",
        );
        let changed = diff(&joined, &alone);
        let summaries: Vec<&str> = changed
            .changes
            .iter()
            .map(|change| change.summary.as_str())
            .collect();
        assert!(
            summaries.contains(&"Hash Left Join removed"),
            "{summaries:?}"
        );
        assert!(
            summaries.contains(&"Seq Scan on customers c removed"),
            "{summaries:?}"
        );
    }

    #[test]
    fn names_partitions_by_what_they_share() {
        assert_eq!(
            common_name(["events_2025_01", "events_2025_02"].into_iter()),
            "events_2025_0*"
        );
        assert_eq!(common_name(["orders"].into_iter()), "orders");
    }
}
