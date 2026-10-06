//! Numbers derived from a plan: how long each node took by itself, what it
//! read, how far the planner's row estimates were off, and where the
//! statement's time went.
//!
//! PostgreSQL reports times and rows as averages per loop and buffers as
//! totals across loops, and a node's figures include those of its children.
//! Exclusive figures therefore come from subtracting the children, with
//! exceptions that follow from how the executor runs a plan:
//!
//! - **Parallel query.** Below a `Gather` or `Gather Merge`, `loops` counts
//!   the processes that ran a node side by side. A node's wall-clock time is
//!   its time per loop × loops ÷ the number of processes; its CPU time is not
//!   divided.
//! - **CTEs.** A CTE runs as the `CTE Scan`s reading it pull rows, so its time
//!   is already inside those scans. It is not subtracted from the node it is
//!   listed under; the scans subtract it instead, in proportion to their own
//!   time (the scan that pulls rows first computes them, later ones read them
//!   from the CTE's store).
//! - **InitPlans.** An InitPlan runs when its result is first needed, inside
//!   the node that needs it. That node subtracts it, rather than the node the
//!   InitPlan is listed under. It is found by the reference to the InitPlan's
//!   result: `$0`, or `(InitPlan 1).col1` from PostgreSQL 17.
//! - **SubPlans** run from the expressions of the node they are listed under,
//!   which subtracts them like any child.
//!
//! Time outside the tree (triggers, serialization, executor startup and
//! shutdown) is accounted for at the statement level.

use serde::Serialize;
use serde_json::Value;

use crate::ir::{Buffers, IoTimings, Node, NodeId, Plan, Relationship};

/// Derived figures for a whole plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    /// One entry per plan node, indexed like [`Plan::nodes`].
    pub nodes: Vec<NodeMetrics>,
    pub statement: StatementMetrics,
}

impl Metrics {
    pub fn node(&self, id: NodeId) -> &NodeMetrics {
        &self.nodes[id.index()]
    }
}

/// Derived figures for one node. Times are in milliseconds and cover all
/// loops; `None` means the plan has no timing for the node (no `ANALYZE`, or
/// `TIMING OFF`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NodeMetrics {
    /// Wall-clock time spent in the node and below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusive_time: Option<f64>,
    /// Wall-clock time spent in the node itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusive_time: Option<f64>,
    /// `exclusive_time` as a fraction of the statement's total time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_share: Option<f64>,
    /// CPU time spent in the node and below it, summed over the processes
    /// that ran it. Equal to the wall-clock time outside parallel sections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inclusive_cpu_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusive_cpu_time: Option<f64>,
    /// Buffers used by the node itself, totals across loops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusive_buffers: Option<Buffers>,
    /// Time the node itself spent reading and writing blocks and temporary
    /// files (`track_io_timing`), summed over the processes that ran it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusive_io_time: Option<f64>,
    /// How many processes ran the node side by side: more than 1 below a
    /// `Gather`.
    pub processes: f64,
    /// Rows returned over all loops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misestimate: Option<Misestimate>,
    /// A node above can stop this one before it returns all its rows (a
    /// `Limit`, a semi or anti join, a merge join, a subquery): returning
    /// fewer rows than estimated then says nothing about the estimate.
    pub may_stop_early: bool,
    /// The children's time exceeds the node's by more than rounding
    /// explains; `exclusive_time` is shown as 0.
    pub inconsistent: bool,
}

/// How far the actual rows per loop are from the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Misestimate {
    /// The larger of actual and estimated rows divided by the smaller, both
    /// counted as at least 1 row; 1 means exact.
    pub factor: f64,
    /// More rows than estimated.
    pub underestimated: bool,
}

/// Figures for the whole statement, in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StatementMetrics {
    /// What shares are relative to: the execution time, or the root node's
    /// inclusive time when the plan does not report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time: Option<f64>,
    /// Time spent in the plan tree: the root node's inclusive time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_time: Option<f64>,
    /// Time spent in triggers, which no node includes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_time: Option<f64>,
    /// Time spent converting the result to the wire format (`SERIALIZE`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialization_time: Option<f64>,
    /// Execution time outside the tree, triggers and serialization: executor
    /// startup and shutdown, including any JIT compilation done then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unattributed_time: Option<f64>,
    /// JIT compilation, which may fall inside node times or outside them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jit_time: Option<f64>,
    /// Time spent on block I/O (`track_io_timing`), summed over the
    /// processes that ran the plan; `None` when the plan reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io: Option<IoTime>,
    /// Nodes ordered by exclusive time, largest first; by exclusive buffers
    /// when the plan has no timing. Nodes with nothing to show are left out.
    pub hotspots: Vec<NodeId>,
}

/// Time spent on block I/O, in milliseconds, summed over the processes that
/// ran the plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct IoTime {
    /// Reading table and index pages that were not in shared buffers.
    pub read: f64,
    /// Writing out pages, as when making room in shared buffers.
    pub write: f64,
    /// Reading and writing temporary files.
    pub temp: f64,
    /// All of it, as a fraction of the time the processes spent in the
    /// plan; `None` without timing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
}

impl IoTime {
    pub fn total(&self) -> f64 {
        self.read + self.write + self.temp
    }
}

/// Times are printed with three decimals, so each per-loop time may be off
/// by half of the last digit.
const ROUNDING: f64 = 0.0005;

/// Computes the derived figures of a plan.
pub fn compute(plan: &Plan) -> Metrics {
    let processes = processes(plan);
    let mut nodes: Vec<NodeMetrics> = plan
        .nodes
        .iter()
        .map(|node| NodeMetrics {
            processes: processes[node.id.index()],
            total_rows: node
                .actuals
                .map(|actuals| actuals.rows * actuals.loops as f64),
            misestimate: misestimate(node),
            may_stop_early: may_stop_early(plan, node),
            ..NodeMetrics::default()
        })
        .collect();

    // Inclusive times, children before parents. Printed times are rounded
    // per loop, so over many loops they drift: a parent can show less time
    // than its children together (a Memoize hit prints as "0.001 ms" over
    // 20,000 loops). Within rounding, the gap is closed by moving the least
    // precise figures, never below what their own children need.
    let mut floor = vec![0.0; plan.nodes.len()];
    for (_, node) in plan.walk().into_iter().rev() {
        let id = node.id.index();
        nodes[id].inclusive_time = inclusive_time(node, processes[id]);
        let children: Vec<usize> = plan
            .children(node.id)
            .filter(|child| matches!(edge(child), Edge::Child))
            .map(|child| child.id.index())
            .collect();
        let sum: f64 = children
            .iter()
            .filter_map(|&child| nodes[child].inclusive_time)
            .sum();
        if let Some(time) = nodes[id].inclusive_time {
            let gap = sum - time;
            if gap > 0.0 {
                let up = rounding(node, processes[id]);
                let downs: Vec<f64> = children
                    .iter()
                    .map(|&child| {
                        let slack = nodes[child].inclusive_time.unwrap_or(0.0) - floor[child];
                        rounding(plan.node(NodeId(child as u32)), processes[child])
                            .min(slack.max(0.0))
                    })
                    .collect();
                let available = up + downs.iter().sum::<f64>();
                if gap <= available {
                    nodes[id].inclusive_time = Some(time + gap * up / available);
                    for (&child, down) in children.iter().zip(&downs) {
                        if let Some(child_time) = nodes[child].inclusive_time.as_mut() {
                            *child_time -= gap * down / available;
                        }
                    }
                }
            }
        }
        floor[id] = children
            .iter()
            .filter_map(|&child| nodes[child].inclusive_time)
            .sum();
    }
    for metrics in &mut nodes {
        metrics.inclusive_cpu_time = metrics.inclusive_time.map(|time| time * metrics.processes);
    }

    // What each node pays for besides its regular children: the CTEs its
    // scans read and the InitPlans it evaluates.
    let mut paid = vec![Paid::default(); plan.nodes.len()];
    for node in &plan.nodes {
        for child in plan.children(node.id) {
            match edge(child) {
                Edge::Child => {}
                Edge::Cte(name) => charge_cte(plan, &nodes, node, child, name, &mut paid),
                Edge::InitPlan => {
                    let evaluator = initplan_evaluator(plan, node, child);
                    paid[evaluator.index()].add(child, &nodes[child.id.index()], 1.0);
                }
            }
        }
    }

    for node in &plan.nodes {
        let id = node.id.index();
        let children = Children::of(plan, node, &nodes);
        let own = &nodes[id];
        let exclusive_wall = own
            .inclusive_time
            .map(|time| time - children.wall - paid[id].wall);
        let exclusive_cpu = own.inclusive_cpu_time.map(|time| {
            if children.parallel {
                // The node runs in one process, its children in several.
                exclusive_wall.unwrap_or(0.0)
            } else {
                time - children.cpu - paid[id].cpu
            }
        });
        let exclusive_buffers = node.buffers.map(|buffers| {
            let mut rest = subtract(buffers, paid[id].buffers);
            for child in plan.children(node.id) {
                if let (Edge::Child, Some(child_buffers)) = (edge(child), child.buffers) {
                    rest = subtract(rest, child_buffers);
                }
            }
            rest
        });
        let exclusive_io_time = node.io_timings.map(|timings| {
            let children: f64 = plan
                .children(node.id)
                .filter(|child| matches!(edge(child), Edge::Child))
                .filter_map(|child| child.io_timings)
                .map(|timings| io_total(&timings))
                .sum();
            (io_total(&timings) - children - paid[id].io).max(0.0)
        });
        let tolerance = children.rounding + paid[id].rounding;

        let metrics = &mut nodes[id];
        metrics.inconsistent = exclusive_wall.is_some_and(|time| time < -tolerance);
        metrics.exclusive_time = exclusive_wall.map(|time| time.max(0.0));
        metrics.exclusive_cpu_time = exclusive_cpu.map(|time| time.max(0.0));
        metrics.exclusive_buffers = exclusive_buffers;
        metrics.exclusive_io_time = exclusive_io_time;
    }

    let statement = statement(plan, &mut nodes);
    Metrics { nodes, statement }
}

/// Totals over a node's regular children (not CTEs or InitPlans), and how
/// much rounding of printed times the node's own time and theirs allow.
struct Children {
    wall: f64,
    cpu: f64,
    rounding: f64,
    /// The children run in more processes than the node: it is a Gather.
    parallel: bool,
}

impl Children {
    fn of(plan: &Plan, node: &Node, metrics: &[NodeMetrics]) -> Self {
        let own = &metrics[node.id.index()];
        let mut children = Children {
            wall: 0.0,
            cpu: 0.0,
            rounding: rounding(node, own.processes),
            parallel: false,
        };
        for child in plan.children(node.id) {
            if !matches!(edge(child), Edge::Child) {
                continue;
            }
            let child_metrics = &metrics[child.id.index()];
            children.wall += child_metrics.inclusive_time.unwrap_or(0.0);
            children.cpu += child_metrics.inclusive_cpu_time.unwrap_or(0.0);
            children.rounding += rounding(child, child_metrics.processes);
            children.parallel |= child_metrics.processes > own.processes;
        }
        children
    }
}

/// How far a node's printed total time may be from the truth: half of the
/// last printed digit, per loop.
fn rounding(node: &Node, processes: f64) -> f64 {
    ROUNDING * loops(node) as f64 / processes + 0.001
}

/// Number of processes running each node side by side: below a Gather, the
/// loops of the Gather's child per loop of the Gather (workers plus the
/// leader, when it takes part).
fn processes(plan: &Plan) -> Vec<f64> {
    let mut processes = vec![1.0; plan.nodes.len()];
    for (_, node) in plan.walk() {
        let Some(parent_id) = node.parent else {
            continue;
        };
        let parent = plan.node(parent_id);
        processes[node.id.index()] = match node.relationship {
            // InitPlans run in the leader before the workers start.
            Some(Relationship::InitPlan) => 1.0,
            _ if parent.node_type.starts_with("Gather") => match (parent.actuals, node.actuals) {
                (Some(gather), Some(child)) if gather.loops > 0 && child.loops > 0 => {
                    (child.loops as f64 / gather.loops as f64).max(1.0)
                }
                _ => 1.0,
            },
            _ => processes[parent_id.index()],
        };
    }
    processes
}

fn loops(node: &Node) -> u64 {
    node.actuals.map_or(0, |actuals| actuals.loops)
}

/// Wall-clock time of a node, including its children, as printed.
fn inclusive_time(node: &Node, processes: f64) -> Option<f64> {
    let actuals = node.actuals?;
    if actuals.never_executed() {
        return Some(0.0);
    }
    actuals
        .total_time
        .map(|time| time * actuals.loops as f64 / processes)
}

fn misestimate(node: &Node) -> Option<Misestimate> {
    let (estimates, actuals) = (node.estimates?, node.actuals?);
    if actuals.never_executed() {
        return None;
    }
    let (estimated, actual) = (estimates.rows.max(1.0), actuals.rows.max(1.0));
    Some(Misestimate {
        factor: estimated.max(actual) / estimated.min(actual),
        underestimated: actual > estimated,
    })
}

/// Whether a node above can stop this one early. Nodes that consume their
/// whole input (a sort, a hash, a plain or hashed aggregate) shield what is
/// below them.
fn may_stop_early(plan: &Plan, node: &Node) -> bool {
    let mut current = node;
    while let Some(parent_id) = current.parent {
        let parent = plan.node(parent_id);
        if matches!(
            current.relationship,
            Some(Relationship::SubPlan | Relationship::InitPlan)
        ) {
            return true;
        }
        let join_type = parent.join_type.as_deref().unwrap_or("");
        let inner = current.relationship == Some(Relationship::Inner);
        match parent.node_type.as_str() {
            "Limit" | "Merge Join" => return true,
            _ if inner && matches!(join_type, "Semi" | "Anti") => return true,
            "Sort" | "Hash" | "SetOp" => return false,
            "Aggregate" if parent.strategy.as_deref() != Some("Sorted") => return false,
            _ => {}
        }
        current = parent;
    }
    false
}

/// How a child relates to its parent for the purpose of subtraction.
enum Edge<'a> {
    Child,
    /// A CTE's subtree, with the CTE's name.
    Cte(&'a str),
    InitPlan,
}

fn edge(child: &Node) -> Edge<'_> {
    if child.relationship != Some(Relationship::InitPlan) {
        return Edge::Child;
    }
    match child
        .subplan_name
        .as_deref()
        .and_then(|name| name.strip_prefix("CTE "))
    {
        Some(name) => Edge::Cte(name),
        None => Edge::InitPlan,
    }
}

/// Time and buffers a node pays for besides its regular children.
#[derive(Clone, Default)]
struct Paid {
    wall: f64,
    cpu: f64,
    buffers: Buffers,
    io: f64,
    /// Rounding allowed for the times paid.
    rounding: f64,
}

impl Paid {
    /// Adds a share of a subtree, given by its root.
    fn add(&mut self, root: &Node, metrics: &NodeMetrics, fraction: f64) {
        self.wall += metrics.inclusive_time.unwrap_or(0.0) * fraction;
        self.cpu += metrics.inclusive_cpu_time.unwrap_or(0.0) * fraction;
        self.rounding += rounding(root, metrics.processes) * fraction;
        if let Some(buffers) = root.buffers {
            self.buffers = add(self.buffers, scale(buffers, fraction));
        }
        if let Some(timings) = root.io_timings {
            self.io += io_total(&timings) * fraction;
        }
    }
}

/// Charges a CTE to the scans that read it: in proportion to their time,
/// which includes computing the CTE's rows, or to the rows they read when
/// the plan has no timing.
fn charge_cte(
    plan: &Plan,
    metrics: &[NodeMetrics],
    owner: &Node,
    cte: &Node,
    name: &str,
    paid: &mut [Paid],
) {
    let scans: Vec<&Node> = subtree(plan, owner.id, Some(cte.id))
        .into_iter()
        .map(|id| plan.node(id))
        .filter(|node| node.node_type == "CTE Scan" && node.cte_name.as_deref() == Some(name))
        .filter(|node| loops(node) > 0)
        .collect();
    let timed: Option<Vec<f64>> = scans
        .iter()
        .map(|scan| metrics[scan.id.index()].inclusive_time)
        .collect();
    let weights = match timed {
        Some(times) if times.iter().sum::<f64>() > 0.0 => times,
        _ => scans
            .iter()
            .map(|scan| {
                let actuals = scan.actuals.expect("executed");
                (actuals.rows + scan.rows_removed_by_filter) * actuals.loops as f64
            })
            .collect(),
    };
    let total: f64 = weights.iter().sum();
    for (scan, weight) in scans.iter().zip(&weights) {
        let fraction = if total > 0.0 {
            weight / total
        } else {
            1.0 / scans.len() as f64
        };
        paid[scan.id.index()].add(cte, &metrics[cte.id.index()], fraction);
    }
}

/// The node that evaluates an InitPlan: the first one, in execution order,
/// whose expressions refer to its result. Falls back to the node the
/// InitPlan is listed under.
fn initplan_evaluator(plan: &Plan, owner: &Node, initplan: &Node) -> NodeId {
    let needles = initplan_references(initplan.subplan_name.as_deref().unwrap_or(""));
    if needles.is_empty() {
        return owner.id;
    }
    // Post-order approximates execution order: the executor pulls rows from
    // the leftmost leaves first.
    subtree(plan, owner.id, Some(initplan.id))
        .into_iter()
        .rev()
        .find(|&id| refers_to(plan.node(id), &needles))
        .unwrap_or(owner.id)
}

/// How expressions refer to an InitPlan's result: `(InitPlan 1)` from
/// PostgreSQL 17, the parameters in `InitPlan 1 (returns $0,$1)` before.
fn initplan_references(name: &str) -> Vec<String> {
    let mut needles = Vec::new();
    if let Some((label, returns)) = name.split_once(" (returns ") {
        needles.push(format!("({label})"));
        for param in returns.trim_end_matches(')').split(',') {
            let param = param.trim();
            if param.starts_with('$') {
                needles.push(param.to_owned());
            }
        }
    } else if !name.is_empty() {
        needles.push(format!("({name})"));
    }
    needles
}

/// Whether any expression of the node contains one of the references. A `$1`
/// must not be followed by another digit.
fn refers_to(node: &Node, needles: &[String]) -> bool {
    let mut texts: Vec<&str> = node.predicates.iter().map(|p| p.text.as_str()).collect();
    for list in [
        &node.output,
        &node.sort_key,
        &node.presorted_key,
        &node.group_key,
    ] {
        texts.extend(list.iter().map(String::as_str));
    }
    for value in node.extra.values() {
        match value {
            Value::String(text) => texts.push(text),
            Value::Array(items) => texts.extend(items.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    texts.iter().any(|text| {
        needles.iter().any(|needle| {
            text.match_indices(needle.as_str()).any(|(at, _)| {
                !needle.starts_with('$')
                    || !text[at + needle.len()..].starts_with(|c: char| c.is_ascii_digit())
            })
        })
    })
}

/// The nodes of a subtree in pre-order, leaving out the subtree of `skip`.
fn subtree(plan: &Plan, root: NodeId, skip: Option<NodeId>) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if Some(id) == skip {
            continue;
        }
        order.push(id);
        stack.extend(plan.node(id).children.iter().rev());
    }
    order
}

fn statement(plan: &Plan, nodes: &mut [NodeMetrics]) -> StatementMetrics {
    let summary = &plan.summary;
    let tree_time = nodes.first().and_then(|root| root.inclusive_time);
    let trigger_time = summary
        .triggers
        .iter()
        .filter_map(|trigger| trigger.time)
        .reduce(|a, b| a + b);
    let serialization_time = summary
        .serialization
        .as_ref()
        .and_then(|serialization| serialization.time);
    let unattributed_time = match (summary.execution_time, tree_time) {
        (Some(execution), Some(tree)) => Some(
            (execution - tree - trigger_time.unwrap_or(0.0) - serialization_time.unwrap_or(0.0))
                .max(0.0),
        ),
        _ => None,
    };
    let total_time = summary
        .execution_time
        .or(tree_time)
        .filter(|&time| time > 0.0);
    if let Some(total) = total_time {
        for metrics in nodes.iter_mut() {
            metrics.time_share = metrics.exclusive_time.map(|time| time / total);
        }
    }

    let mut hotspots: Vec<(NodeId, f64)> = plan
        .nodes
        .iter()
        .filter_map(|node| {
            let metrics = &nodes[node.id.index()];
            let weight = match tree_time {
                Some(_) => metrics.exclusive_time,
                None => metrics
                    .exclusive_buffers
                    .map(|buffers| blocks(&buffers) as f64),
            }?;
            (weight > 0.0).then_some((node.id, weight))
        })
        .collect();
    hotspots.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    // The root's I/O includes every node's, in every process: compared
    // with the time of every process, the nodes' own CPU time summed.
    let io = plan.root().io_timings.map(|timings| {
        let cpu = nodes
            .iter()
            .filter_map(|metrics| metrics.exclusive_cpu_time)
            .reduce(|a, b| a + b);
        let mut io = IoTime {
            read: timings.shared_read + timings.local_read,
            write: timings.shared_write + timings.local_write,
            temp: timings.temp_read + timings.temp_write,
            share: None,
        };
        io.share = cpu
            .filter(|&cpu| cpu > 0.0)
            .map(|cpu| (io.total() / cpu).min(1.0));
        io
    });

    StatementMetrics {
        total_time,
        planning_time: summary.planning_time,
        execution_time: summary.execution_time,
        tree_time,
        trigger_time,
        serialization_time,
        unattributed_time,
        jit_time: summary
            .jit
            .as_ref()
            .and_then(|jit| jit.timing)
            .map(|timing| timing.total),
        io,
        hotspots: hotspots.into_iter().map(|(id, _)| id).collect(),
    }
}

/// All the I/O time of a node and below it.
fn io_total(timings: &IoTimings) -> f64 {
    timings.shared_read
        + timings.shared_write
        + timings.local_read
        + timings.local_write
        + timings.temp_read
        + timings.temp_write
}

/// Shared and local blocks hit or read: the pages a node touched.
pub fn blocks(buffers: &Buffers) -> u64 {
    buffers.shared_hit + buffers.shared_read + buffers.local_hit + buffers.local_read
}

fn subtract(a: Buffers, b: Buffers) -> Buffers {
    Buffers {
        shared_hit: a.shared_hit.saturating_sub(b.shared_hit),
        shared_read: a.shared_read.saturating_sub(b.shared_read),
        shared_dirtied: a.shared_dirtied.saturating_sub(b.shared_dirtied),
        shared_written: a.shared_written.saturating_sub(b.shared_written),
        local_hit: a.local_hit.saturating_sub(b.local_hit),
        local_read: a.local_read.saturating_sub(b.local_read),
        local_dirtied: a.local_dirtied.saturating_sub(b.local_dirtied),
        local_written: a.local_written.saturating_sub(b.local_written),
        temp_read: a.temp_read.saturating_sub(b.temp_read),
        temp_written: a.temp_written.saturating_sub(b.temp_written),
    }
}

fn add(a: Buffers, b: Buffers) -> Buffers {
    Buffers {
        shared_hit: a.shared_hit + b.shared_hit,
        shared_read: a.shared_read + b.shared_read,
        shared_dirtied: a.shared_dirtied + b.shared_dirtied,
        shared_written: a.shared_written + b.shared_written,
        local_hit: a.local_hit + b.local_hit,
        local_read: a.local_read + b.local_read,
        local_dirtied: a.local_dirtied + b.local_dirtied,
        local_written: a.local_written + b.local_written,
        temp_read: a.temp_read + b.temp_read,
        temp_written: a.temp_written + b.temp_written,
    }
}

/// A share of a buffer count, rounded to whole blocks.
fn scale(buffers: Buffers, fraction: f64) -> Buffers {
    // Block counts and fractions in [0, 1]: the result fits and is not negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let part = |count: u64| (count as f64 * fraction).round() as u64;
    Buffers {
        shared_hit: part(buffers.shared_hit),
        shared_read: part(buffers.shared_read),
        shared_dirtied: part(buffers.shared_dirtied),
        shared_written: part(buffers.shared_written),
        local_hit: part(buffers.local_hit),
        local_read: part(buffers.local_read),
        local_dirtied: part(buffers.local_dirtied),
        local_written: part(buffers.local_written),
        temp_read: part(buffers.temp_read),
        temp_written: part(buffers.temp_written),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute_for(text: &str) -> (Plan, Metrics) {
        let plan = crate::parse(text).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        let metrics = compute(&plan);
        (plan, metrics)
    }

    fn exclusive(metrics: &Metrics) -> Vec<f64> {
        metrics
            .nodes
            .iter()
            .map(|node| (node.exclusive_time.unwrap() * 1000.0).round() / 1000.0)
            .collect()
    }

    #[test]
    fn splits_io_time_between_nodes() {
        let plan = crate::parse(
            "\
Sort  (cost=38438.14..38938.14 rows=200000 width=37) (actual time=340.230..372.433 rows=200000 loops=1)
  Sort Key: orders.note
  Sort Method: external merge  Disk: 9272kB
  Buffers: shared hit=2130 read=290, temp read=4609 written=4921
  I/O Timings: shared read=1.647, temp read=7.540 write=8.700
  ->  Seq Scan on orders  (cost=0.00..4417.00 rows=200000 width=37) (actual time=0.014..22.127 rows=200000 loops=1)
        Buffers: shared hit=2127 read=290
        I/O Timings: shared read=1.647
Execution Time: 379.494 ms",
        )
        .unwrap();
        let metrics = compute(&plan);
        let sort = metrics.node(NodeId(0)).exclusive_io_time.unwrap();
        assert!((sort - 16.24).abs() < 1e-9, "{sort}");
        assert_eq!(metrics.node(NodeId(1)).exclusive_io_time, Some(1.647));
        let io = metrics.statement.io.unwrap();
        assert_eq!((io.read, io.write), (1.647, 0.0));
        assert!((io.temp - 16.24).abs() < 1e-9);
        // Of the 372 ms the plan took.
        assert!((io.share.unwrap() - 17.887 / 372.433).abs() < 1e-9);
    }

    #[test]
    fn compares_io_with_the_time_of_every_process() {
        // Three processes read for most of their 100 ms each.
        let plan = crate::parse(
            "\
Gather  (cost=1000.00..5000.00 rows=10 width=64) (actual time=1.000..100.000 rows=10 loops=1)
  Workers Planned: 2
  Workers Launched: 2
  Buffers: shared read=3000
  I/O Timings: shared read=240.000
  ->  Parallel Seq Scan on orders  (cost=0.00..4000.00 rows=4 width=64) (actual time=1.000..99.000 rows=3 loops=3)
        Buffers: shared read=3000
        I/O Timings: shared read=240.000
Execution Time: 100.500 ms",
        )
        .unwrap();
        let io = compute(&plan).statement.io.unwrap();
        let share = io.share.unwrap();
        assert!((0.75..0.85).contains(&share), "{share}");
    }

    #[test]
    fn subtracts_children_from_time_and_buffers() {
        let (plan, metrics) = compute_for(
            "\
Hash Join  (cost=1.00..10.00 rows=10 width=8) (actual time=0.100..5.000 rows=10 loops=1)
  Hash Cond: (a.id = b.id)
  Buffers: shared hit=100 read=5
  ->  Seq Scan on a  (cost=0.00..5.00 rows=100 width=4) (actual time=0.010..2.500 rows=100 loops=1)
        Buffers: shared hit=60 read=5
  ->  Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=1.000..1.000 rows=10 loops=1)
        Buckets: 1024  Batches: 1  Memory Usage: 9kB
        Buffers: shared hit=40
        ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.500 rows=10 loops=1)
              Buffers: shared hit=40
Execution Time: 5.500 ms",
        );
        assert_eq!(exclusive(&metrics), [1.5, 2.5, 0.5, 0.5]);
        let join = metrics.node(NodeId(0));
        assert_eq!(join.exclusive_buffers, Some(Buffers::default()));
        assert_eq!(join.time_share, Some(1.5 / 5.5));
        assert_eq!(
            metrics.statement.hotspots,
            [NodeId(1), NodeId(0), NodeId(2), NodeId(3)]
        );
        assert_eq!(metrics.statement.tree_time, Some(5.0));
        assert!((metrics.statement.unattributed_time.unwrap() - 0.5).abs() < 1e-9);
        assert!(
            plan.nodes
                .iter()
                .all(|node| !metrics.node(node.id).inconsistent)
        );
    }

    #[test]
    fn multiplies_time_by_loops_but_not_buffers() {
        let (_, metrics) = compute_for(
            "\
Nested Loop  (cost=0.29..90.00 rows=10 width=8) (actual time=0.020..3.000 rows=10 loops=1)
  Buffers: shared hit=35
  ->  Seq Scan on a  (cost=0.00..1.10 rows=10 width=4) (actual time=0.005..0.050 rows=10 loops=1)
        Buffers: shared hit=5
  ->  Index Scan using b_pkey on b  (cost=0.29..8.30 rows=1 width=4) (actual time=0.200..0.250 rows=1 loops=10)
        Index Cond: (id = a.id)
        Buffers: shared hit=30",
        );
        assert_eq!(exclusive(&metrics), [0.45, 0.05, 2.5]);
        assert_eq!(metrics.node(NodeId(2)).total_rows, Some(10.0));
        assert_eq!(
            metrics
                .node(NodeId(0))
                .exclusive_buffers
                .unwrap()
                .shared_hit,
            0
        );
    }

    #[test]
    fn divides_parallel_time_by_the_processes() {
        let (_, metrics) = compute_for(
            "\
Gather  (cost=1000.00..2000.00 rows=100 width=4) (actual time=0.500..12.000 rows=100 loops=1)
  Workers Planned: 2
  Workers Launched: 2
  ->  Parallel Seq Scan on t  (cost=0.00..1000.00 rows=42 width=4) (actual time=0.010..10.000 rows=33 loops=3)",
        );
        let (gather, scan) = (metrics.node(NodeId(0)), metrics.node(NodeId(1)));
        assert_eq!(scan.processes, 3.0);
        assert_eq!(
            (scan.inclusive_time, scan.inclusive_cpu_time),
            (Some(10.0), Some(30.0))
        );
        assert_eq!(gather.exclusive_time, Some(2.0));
        assert_eq!(gather.exclusive_cpu_time, Some(2.0));
        assert!(!gather.inconsistent);
    }

    #[test]
    fn charges_a_cte_to_the_scans_that_read_it() {
        let (_, metrics) = compute_for(
            "\
Nested Loop  (cost=10.00..20.00 rows=4 width=16) (actual time=5.000..9.000 rows=4 loops=1)
  CTE c
    ->  Seq Scan on t  (cost=0.00..10.00 rows=2 width=8) (actual time=0.100..6.000 rows=2 loops=1)
  ->  CTE Scan on c a  (cost=0.00..0.04 rows=2 width=8) (actual time=0.200..6.500 rows=2 loops=1)
  ->  CTE Scan on c b  (cost=0.00..0.04 rows=2 width=8) (actual time=0.001..0.002 rows=2 loops=2)",
        );
        let times = exclusive(&metrics);
        // The CTE is not subtracted from the join it is listed under ...
        assert_eq!(times[0], 2.496);
        // ... but from its scans, mostly from the one that computed its rows.
        assert_eq!(times[2], 0.504);
        assert_eq!(times[3], 0.0);
        let total: f64 = metrics
            .nodes
            .iter()
            .filter_map(|node| node.exclusive_time)
            .sum();
        assert!((total - 9.0).abs() < 1e-9);
    }

    #[test]
    fn charges_an_initplan_to_the_node_that_reads_its_result() {
        for reference in ["(InitPlan 1).col1", "$0"] {
            let label = if reference == "$0" {
                "InitPlan 1 (returns $0)"
            } else {
                "InitPlan 1"
            };
            let (_, metrics) = compute_for(&format!(
                "\
Hash Join  (cost=1.00..10.00 rows=1 width=8) (actual time=1.000..9.000 rows=1 loops=1)
  Hash Cond: (a.id = b.id)
  {label}
    ->  Result  (cost=0.00..0.01 rows=1 width=4) (actual time=2.000..2.000 rows=1 loops=1)
  ->  Seq Scan on a  (cost=0.00..5.00 rows=10 width=4) (actual time=2.100..5.000 rows=10 loops=1)
        Filter: (x > {reference})
  ->  Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=1.000..1.000 rows=10 loops=1)
        ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.500 rows=10 loops=1)"
            ));
            assert_eq!(
                exclusive(&metrics),
                [3.0, 2.0, 3.0, 0.5, 0.5],
                "{reference}"
            );
        }
    }

    #[test]
    fn subtracts_subplans_from_the_node_they_run_in() {
        let (_, metrics) = compute_for(
            "\
Seq Scan on t  (cost=0.00..100.00 rows=10 width=4) (actual time=0.100..10.000 rows=10 loops=1)
  Filter: (SubPlan 1)
  SubPlan 1
    ->  Index Scan using u_pkey on u  (cost=0.29..8.30 rows=1 width=4) (actual time=0.500..0.800 rows=1 loops=10)
          Index Cond: (id = t.id)",
        );
        assert_eq!(exclusive(&metrics), [2.0, 8.0]);
    }

    #[test]
    fn absorbs_rounding_in_the_least_precise_figures() {
        // 0.001 ms per loop over 20,000 loops is anything from 10 to 30 ms.
        let (_, metrics) = compute_for(
            "\
Nested Loop  (cost=0.71..2717.66 rows=20000 width=16) (actual time=0.030..17.853 rows=20000 loops=1)
  ->  Index Scan using oi_pkey on oi  (cost=0.42..696.66 rows=20000 width=8) (actual time=0.010..3.031 rows=20000 loops=1)
  ->  Memoize  (cost=0.29..0.32 rows=1 width=16) (actual time=0.001..0.001 rows=1 loops=20000)
        Cache Key: oi.pid
        ->  Index Scan using p_pkey on p  (cost=0.28..0.31 rows=1 width=16) (actual time=0.001..0.001 rows=1 loops=5000)
              Index Cond: (id = oi.pid)
Execution Time: 18.902 ms",
        );
        let tree = metrics.statement.tree_time.unwrap();
        assert!((tree - 17.853).abs() < 0.01, "{tree}");
        let total: f64 = metrics
            .nodes
            .iter()
            .filter_map(|node| node.exclusive_time)
            .sum();
        assert!((total - tree).abs() < 1e-9);
        assert!(metrics.nodes.iter().all(|node| !node.inconsistent));
        let memoize = metrics.node(NodeId(2)).inclusive_time.unwrap();
        assert!(memoize > 5.0 && memoize < 15.0, "{memoize}");
    }

    #[test]
    fn misestimates_and_early_stops() {
        let (_, metrics) = compute_for(
            "\
Limit  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.050 rows=10 loops=1)
  ->  Seq Scan on t  (cost=0.00..100.00 rows=5000 width=4) (actual time=0.010..0.040 rows=10 loops=1)",
        );
        let scan = metrics.node(NodeId(1));
        assert_eq!(
            scan.misestimate,
            Some(Misestimate {
                factor: 500.0,
                underestimated: false
            })
        );
        assert!(scan.may_stop_early);
        assert!(!metrics.node(NodeId(0)).may_stop_early);

        let (_, metrics) = compute_for(
            "\
Nested Loop Semi Join  (cost=0.00..10.00 rows=10 width=4) (actual time=0.010..5.000 rows=1000 loops=1)
  ->  Sort  (cost=0.00..5.00 rows=10 width=4) (actual time=0.010..2.000 rows=1000 loops=1)
        Sort Key: a.x
        ->  Seq Scan on a  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..1.000 rows=1000 loops=1)
  ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4) (actual time=0.001..0.002 rows=1 loops=1000)",
        );
        let outer = metrics.node(NodeId(1));
        assert_eq!(
            outer.misestimate,
            Some(Misestimate {
                factor: 100.0,
                underestimated: true
            })
        );
        assert!(!outer.may_stop_early);
        // The sort reads all of its input, whatever happens above it.
        assert!(!metrics.node(NodeId(2)).may_stop_early);
        assert!(metrics.node(NodeId(3)).may_stop_early);
    }

    #[test]
    fn accounts_for_time_outside_the_tree() {
        let (_, metrics) = compute_for(
            "\
Delete on t  (cost=0.00..8.30 rows=0 width=0) (actual time=0.100..0.100 rows=0 loops=1)
  ->  Index Scan using t_pkey on t  (cost=0.29..8.30 rows=1 width=6) (actual time=0.010..0.020 rows=1 loops=1)
        Index Cond: (id = 1)
Planning Time: 0.100 ms
Trigger for constraint u_t_fk: time=50.000 calls=1
Execution Time: 50.500 ms",
        );
        let statement = &metrics.statement;
        assert_eq!(statement.trigger_time, Some(50.0));
        assert_eq!(statement.total_time, Some(50.5));
        assert!((statement.unattributed_time.unwrap() - 0.4).abs() < 1e-9);
        assert!(metrics.node(NodeId(0)).time_share.unwrap() < 0.01);
    }

    #[test]
    fn works_without_timing() {
        let (_, metrics) =
            compute_for("Seq Scan on t  (cost=0.00..100.00 rows=5000 width=4)\n  Filter: (a = 1)");
        let scan = metrics.node(NodeId(0));
        assert_eq!(
            (scan.inclusive_time, scan.exclusive_time, scan.misestimate),
            (None, None, None)
        );
        assert!(metrics.statement.hotspots.is_empty());

        let (_, metrics) = compute_for(
            "\
Seq Scan on t  (cost=0.00..100.00 rows=5000 width=4) (actual rows=50 loops=1)
  Buffers: shared hit=12
Execution Time: 1.000 ms",
        );
        let scan = metrics.node(NodeId(0));
        assert_eq!(scan.exclusive_time, None);
        assert_eq!(scan.misestimate.map(|m| m.factor), Some(100.0));
        // Without timing, hotspots go by buffers.
        assert_eq!(metrics.statement.hotspots, [NodeId(0)]);
    }
}
