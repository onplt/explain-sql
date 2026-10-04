//! The plan IR: what every front end (PostgreSQL JSON, PostgreSQL text, and
//! later other engines) is lowered into, and what the metrics engine, the
//! rules and the user interfaces work on.
//!
//! Nodes live in an arena ([`Plan::nodes`]) and refer to each other through
//! [`NodeId`]s. Properties that later phases rely on are typed; every other
//! property is kept in `extra` under its PostgreSQL JSON name, so nothing in
//! the input is lost.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// A parsed query plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    /// Every node of the plan. `nodes[0]` is the root, and a node's id is
    /// its index.
    pub nodes: Vec<Node>,
    pub summary: Summary,
    pub source: Source,
    /// Problems found while parsing. The plan is usable despite them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl Plan {
    pub fn root(&self) -> &Node {
        &self.nodes[0]
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = &Node> {
        self.node(id).children.iter().map(|&child| self.node(child))
    }

    /// Every node with its depth, in pre-order (a parent before its
    /// children, children in plan order).
    pub fn walk(&self) -> Vec<(usize, &Node)> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![(0, NodeId(0))];
        while let Some((depth, id)) = stack.pop() {
            let node = self.node(id);
            order.push((depth, node));
            for &child in node.children.iter().rev() {
                stack.push((depth + 1, child));
            }
        }
        order
    }
}

/// The position of a node in [`Plan::nodes`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One plan node.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Node {
    pub id: NodeId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<NodeId>,

    /// PostgreSQL's node type, as in JSON plans: `Seq Scan`, `Hash Join`,
    /// `Aggregate`, `ModifyTable`, ...
    pub node_type: String,
    /// How this node relates to its parent; `None` for the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relationship: Option<Relationship>,
    /// `InitPlan 1`, `SubPlan 2`, `CTE totals`, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subplan_name: Option<String>,

    #[serde(skip_serializing_if = "is_false")]
    pub parallel_aware: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub async_capable: bool,
    /// The planner used this node despite an `enable_*` setting (PostgreSQL 18+).
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub inner_unique: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub single_copy: bool,

    /// Joins: `Inner`, `Left`, `Full`, `Right`, `Semi`, `Anti`, `Right Semi`, `Right Anti`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_type: Option<String>,
    /// Aggregates and set operations: `Plain`, `Sorted`, `Hashed`, `Mixed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// Aggregates: `Simple`, `Partial`, `Finalize`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_mode: Option<String>,
    /// Data modification: `Insert`, `Update`, `Delete`, `Merge`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Set operations: `Intersect`, `Intersect All`, `Except`, `Except All`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Index scans: `Forward`, `Backward`, `NoMovement`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_direction: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cte_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_plan_provider: Option<String>,

    /// `None` when the plan was produced with `COSTS OFF`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimates: Option<Estimates>,
    /// `None` without `ANALYZE`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actuals: Option<Actuals>,
    /// `None` when not reported or all zero (text plans omit zero counts);
    /// likewise for `io_timings` and `wal`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffers: Option<Buffers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_timings: Option<IoTimings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal: Option<Wal>,

    /// Conditions in a fixed order (index conditions first, filters last).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub predicates: Vec<Predicate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_key: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub presorted_key: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub group_key: Vec<String>,

    /// Per loop, like `actuals.rows`.
    #[serde(skip_serializing_if = "is_zero")]
    pub rows_removed_by_filter: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub rows_removed_by_join_filter: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub rows_removed_by_index_recheck: f64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers_planned: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers_launched: Option<u32>,
    /// Per-worker details (`VERBOSE` plans).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workers: Vec<Worker>,

    /// Every other property, under its PostgreSQL JSON name. Properties
    /// whose value is the default (zero or false) are left out, because
    /// text plans omit them.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl Node {
    /// The text of the first predicate of the given kind.
    pub fn predicate(&self, kind: PredicateKind) -> Option<&str> {
        self.predicates
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| p.text.as_str())
    }

    pub fn extra_f64(&self, key: &str) -> Option<f64> {
        self.extra.get(key).and_then(Value::as_f64)
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }
}

/// How a node relates to its parent (`Parent Relationship` in JSON plans).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Relationship {
    Outer,
    Inner,
    Member,
    InitPlan,
    SubPlan,
    Subquery,
}

impl Relationship {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Outer" => Relationship::Outer,
            "Inner" => Relationship::Inner,
            "Member" => Relationship::Member,
            "InitPlan" => Relationship::InitPlan,
            "SubPlan" => Relationship::SubPlan,
            "Subquery" => Relationship::Subquery,
            _ => return None,
        })
    }
}

/// The planner's estimates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimates {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub rows: f64,
    pub width: u64,
}

/// What `ANALYZE` measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Actuals {
    /// Average per loop, in milliseconds. `None` with `TIMING OFF` and for
    /// nodes that never ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_time: Option<f64>,
    /// Average per loop; fractional from PostgreSQL 18 on.
    pub rows: f64,
    pub loops: u64,
}

impl Actuals {
    pub fn never_executed(&self) -> bool {
        self.loops == 0
    }
}

/// Buffer counts. Unlike times and rows, these are totals over all loops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Buffers {
    pub shared_hit: u64,
    pub shared_read: u64,
    pub shared_dirtied: u64,
    pub shared_written: u64,
    pub local_hit: u64,
    pub local_read: u64,
    pub local_dirtied: u64,
    pub local_written: u64,
    pub temp_read: u64,
    pub temp_written: u64,
}

/// Time spent on block I/O (`track_io_timing`), in milliseconds. Before
/// PostgreSQL 17, the shared figures include local buffers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct IoTimings {
    pub shared_read: f64,
    pub shared_write: f64,
    pub local_read: f64,
    pub local_write: f64,
    pub temp_read: f64,
    pub temp_write: f64,
}

/// WAL generated by the node (`WAL` option).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Wal {
    pub records: u64,
    pub fpi: u64,
    pub bytes: u64,
    pub buffers_full: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Predicate {
    pub kind: PredicateKind,
    /// The condition as PostgreSQL deparsed it.
    pub text: String,
}

/// The kinds of conditions a node can carry, in the order they are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum PredicateKind {
    #[serde(rename = "Index Cond")]
    IndexCond,
    #[serde(rename = "Recheck Cond")]
    RecheckCond,
    #[serde(rename = "Order By")]
    OrderBy,
    #[serde(rename = "TID Cond")]
    TidCond,
    #[serde(rename = "Hash Cond")]
    HashCond,
    #[serde(rename = "Merge Cond")]
    MergeCond,
    #[serde(rename = "Join Filter")]
    JoinFilter,
    #[serde(rename = "One-Time Filter")]
    OneTimeFilter,
    #[serde(rename = "Run Condition")]
    RunCondition,
    #[serde(rename = "Filter")]
    Filter,
}

impl PredicateKind {
    pub const ALL: [PredicateKind; 10] = [
        PredicateKind::IndexCond,
        PredicateKind::RecheckCond,
        PredicateKind::OrderBy,
        PredicateKind::TidCond,
        PredicateKind::HashCond,
        PredicateKind::MergeCond,
        PredicateKind::JoinFilter,
        PredicateKind::OneTimeFilter,
        PredicateKind::RunCondition,
        PredicateKind::Filter,
    ];

    /// The property name in PostgreSQL plans.
    pub fn key(self) -> &'static str {
        match self {
            PredicateKind::IndexCond => "Index Cond",
            PredicateKind::RecheckCond => "Recheck Cond",
            PredicateKind::OrderBy => "Order By",
            PredicateKind::TidCond => "TID Cond",
            PredicateKind::HashCond => "Hash Cond",
            PredicateKind::MergeCond => "Merge Cond",
            PredicateKind::JoinFilter => "Join Filter",
            PredicateKind::OneTimeFilter => "One-Time Filter",
            PredicateKind::RunCondition => "Run Condition",
            PredicateKind::Filter => "Filter",
        }
    }
}

/// Per-worker statistics of a parallel node.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Worker {
    pub number: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actuals: Option<Actuals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffers: Option<Buffers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_timings: Option<IoTimings>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Everything outside the node tree.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    /// In milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning_time: Option<f64>,
    /// In milliseconds; includes triggers and executor startup, which are
    /// not attributed to any node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning: Option<Planning>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Trigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jit: Option<Jit>,
    /// Non-default planner settings (`SETTINGS` option).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialization: Option<Serialization>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_identifier: Option<i64>,
    /// The statement, when the input carried it (auto_explain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_text: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Resources used while planning (PostgreSQL 13+). `None` in the summary
/// when there is nothing to report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Planning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffers: Option<Buffers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_timings: Option<IoTimings>,
    /// In kilobytes (`MEMORY` option, PostgreSQL 17+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_allocated: Option<f64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Time spent in a trigger, which is not part of any node's time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trigger {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// Total, in milliseconds. `None` with `TIMING OFF`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    pub calls: f64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Just-in-time compilation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Jit {
    pub functions: u64,
    pub inlining: bool,
    pub optimization: bool,
    pub expressions: bool,
    pub deforming: bool,
    /// `None` with `TIMING OFF`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<JitTiming>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// JIT phases, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct JitTiming {
    pub generation: f64,
    /// Part of `generation` (PostgreSQL 17+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deform: Option<f64>,
    pub inlining: f64,
    pub optimization: f64,
    pub emission: f64,
    pub total: f64,
}

/// Cost of converting the result to the wire format (`SERIALIZE` option,
/// PostgreSQL 17+).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Serialization {
    /// In milliseconds; `None` with `TIMING OFF`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// In kilobytes.
    pub output_volume: f64,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffers: Option<Buffers>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// How the input was recognized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    pub format: Format,
    /// Wrappers removed before parsing, outermost first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wrappers: Vec<Wrapper>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Format {
    Json,
    Text,
}

/// Something wrapped around the plan in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Wrapper {
    /// A Markdown code fence.
    MarkdownFence,
    /// A PostgreSQL JSON log record (`log_destination = jsonlog`).
    JsonLog,
    /// An auto_explain entry in a server log.
    AutoExplainLog,
    /// psql's aligned output: header, borders, `+` continuations, row count.
    PsqlTable,
    /// psql's expanded output (`\x`).
    PsqlExpanded,
    /// psql's wrapped output, whose long lines were joined back.
    PsqlWrapped,
}

/// A problem found while parsing. `line` is 1-based and refers to the plan
/// after wrappers were removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}
