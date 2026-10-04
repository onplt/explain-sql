//! Raw trees into the IR.
//!
//! Typed fields are taken out of each node's properties; whatever remains
//! becomes `extra`. Differences that only reflect how a plan was printed are
//! smoothed out here, so that the JSON and text forms of the same plan lower
//! to the same IR: properties and groups (buffers, I/O timings, WAL, the
//! planning section) that the text format omits when they are zero are
//! dropped from both, numbers are put in one canonical form, I/O timing keys
//! of all versions map to the same fields, and the single source node of a
//! data-modifying statement is `Outer` whatever the server version.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

use super::raw::RawPlan;
use crate::ir::{
    Actuals, Buffers, Estimates, IoTimings, Jit, JitTiming, Node, NodeId, Plan, Planning,
    Predicate, PredicateKind, Relationship, Serialization, Source, Summary, Trigger, Wal, Warning,
    Worker,
};

/// Properties that the text format leaves out when they are zero.
const OMITTED_WHEN_ZERO: [&str; 5] = [
    "Subplans Removed",
    "Planned Partitions",
    "Disk Usage",
    "Exact Heap Blocks",
    "Lossy Heap Blocks",
];

pub(crate) fn lower(raw: RawPlan, source: Source, mut warnings: Vec<Warning>) -> Plan {
    let RawPlan {
        top,
        nodes: raw_nodes,
    } = raw;
    let mut nodes = Vec::with_capacity(raw_nodes.len());
    for (index, raw_node) in raw_nodes.into_iter().enumerate() {
        let mut props = raw_node.props;
        let node_type = take_string(&mut props, "Node Type").unwrap_or_else(|| {
            warnings.push(Warning {
                line: raw_node.line,
                message: "a plan node without a node type".to_owned(),
            });
            "Unknown".to_owned()
        });
        let mut node = Node {
            id: node_id(index),
            parent: raw_node.parent.map(node_id),
            children: raw_node.children.into_iter().map(node_id).collect(),
            node_type,
            ..Node::default()
        };
        fill_node(&mut node, &mut props);
        node.extra = canonical(props);
        nodes.push(node);
    }
    source_of_modifications_is_outer(&mut nodes);
    let summary = summary(top);
    Plan {
        nodes,
        summary,
        source,
        warnings,
    }
}

fn node_id(index: usize) -> NodeId {
    NodeId(u32::try_from(index).unwrap_or(u32::MAX))
}

fn fill_node(node: &mut Node, props: &mut Map<String, Value>) {
    node.relationship = match take_string(props, "Parent Relationship") {
        Some(value) => match Relationship::parse(&value) {
            Some(relationship) => Some(relationship),
            // Children of a custom scan.
            None if value == "child" || value == "children" => Some(Relationship::Member),
            None => {
                props.insert("Parent Relationship".to_owned(), Value::String(value));
                None
            }
        },
        None => None,
    };
    node.subplan_name = take_string(props, "Subplan Name");

    node.parallel_aware = take_bool(props, "Parallel Aware").unwrap_or(false);
    node.async_capable = take_bool(props, "Async Capable").unwrap_or(false);
    node.disabled = take_bool(props, "Disabled").unwrap_or(false);
    node.inner_unique = take_bool(props, "Inner Unique").unwrap_or(false);
    node.single_copy = take_bool(props, "Single Copy").unwrap_or(false);

    node.join_type = take_string(props, "Join Type");
    node.strategy = take_string(props, "Strategy");
    node.partial_mode = take_string(props, "Partial Mode");
    node.operation = take_string(props, "Operation");
    node.command = take_string(props, "Command");
    node.scan_direction = take_string(props, "Scan Direction");

    node.schema = take_string(props, "Schema");
    node.relation_name = take_string(props, "Relation Name");
    node.alias = take_string(props, "Alias");
    node.index_name = take_string(props, "Index Name");
    node.cte_name = take_string(props, "CTE Name");
    node.function_name = take_string(props, "Function Name");
    node.custom_plan_provider = take_string(props, "Custom Plan Provider");

    node.estimates = estimates(props);
    node.actuals = actuals(props);
    node.buffers = buffers(props);
    node.io_timings = io_timings(props);
    node.wal = wal(props);

    node.predicates = PredicateKind::ALL
        .into_iter()
        .filter_map(|kind| take_string(props, kind.key()).map(|text| Predicate { kind, text }))
        .collect();
    node.output = take_list(props, "Output");
    node.sort_key = take_list(props, "Sort Key");
    node.presorted_key = take_list(props, "Presorted Key");
    node.group_key = take_list(props, "Group Key");

    node.rows_removed_by_filter = take_f64(props, "Rows Removed by Filter").unwrap_or(0.0);
    node.rows_removed_by_join_filter =
        take_f64(props, "Rows Removed by Join Filter").unwrap_or(0.0);
    node.rows_removed_by_index_recheck =
        take_f64(props, "Rows Removed by Index Recheck").unwrap_or(0.0);

    node.workers_planned = take_u64(props, "Workers Planned").and_then(|n| u32::try_from(n).ok());
    node.workers_launched = take_u64(props, "Workers Launched").and_then(|n| u32::try_from(n).ok());
    node.workers = workers(props);
}

fn estimates(props: &mut Map<String, Value>) -> Option<Estimates> {
    if !props.contains_key("Total Cost") {
        return None;
    }
    Some(Estimates {
        startup_cost: take_f64(props, "Startup Cost").unwrap_or(0.0),
        total_cost: take_f64(props, "Total Cost").unwrap_or(0.0),
        rows: take_f64(props, "Plan Rows").unwrap_or(0.0),
        width: take_u64(props, "Plan Width").unwrap_or(0),
    })
}

fn actuals(props: &mut Map<String, Value>) -> Option<Actuals> {
    if !props.contains_key("Actual Loops") && !props.contains_key("Actual Rows") {
        return None;
    }
    let loops = take_u64(props, "Actual Loops").unwrap_or(1);
    let rows = take_f64(props, "Actual Rows").unwrap_or(0.0);
    let startup_time = take_f64(props, "Actual Startup Time");
    let total_time = take_f64(props, "Actual Total Time");
    // JSON plans print zero times for nodes that never ran; text plans print
    // "(never executed)".
    let ran = loops > 0;
    Some(Actuals {
        startup_time: startup_time.filter(|_| ran),
        total_time: total_time.filter(|_| ran),
        rows,
        loops,
    })
}

const BUFFER_KEYS: [&str; 10] = [
    "Shared Hit Blocks",
    "Shared Read Blocks",
    "Shared Dirtied Blocks",
    "Shared Written Blocks",
    "Local Hit Blocks",
    "Local Read Blocks",
    "Local Dirtied Blocks",
    "Local Written Blocks",
    "Temp Read Blocks",
    "Temp Written Blocks",
];

fn buffers(props: &mut Map<String, Value>) -> Option<Buffers> {
    if !BUFFER_KEYS.iter().any(|key| props.contains_key(*key)) {
        return None;
    }
    let mut take = |key: &str| take_u64(props, key).unwrap_or(0);
    let buffers = Buffers {
        shared_hit: take("Shared Hit Blocks"),
        shared_read: take("Shared Read Blocks"),
        shared_dirtied: take("Shared Dirtied Blocks"),
        shared_written: take("Shared Written Blocks"),
        local_hit: take("Local Hit Blocks"),
        local_read: take("Local Read Blocks"),
        local_dirtied: take("Local Dirtied Blocks"),
        local_written: take("Local Written Blocks"),
        temp_read: take("Temp Read Blocks"),
        temp_written: take("Temp Written Blocks"),
    };
    (buffers != Buffers::default()).then_some(buffers)
}

/// I/O timing keys: `I/O Read Time` (up to PostgreSQL 16, shared and local
/// together) and the split keys of PostgreSQL 17.
fn io_timings(props: &mut Map<String, Value>) -> Option<IoTimings> {
    const KEYS: [&str; 8] = [
        "I/O Read Time",
        "I/O Write Time",
        "Shared I/O Read Time",
        "Shared I/O Write Time",
        "Local I/O Read Time",
        "Local I/O Write Time",
        "Temp I/O Read Time",
        "Temp I/O Write Time",
    ];
    if !KEYS.iter().any(|key| props.contains_key(*key)) {
        return None;
    }
    let mut take = |key: &str| take_f64(props, key).unwrap_or(0.0);
    let old_read = take("I/O Read Time");
    let old_write = take("I/O Write Time");
    let timings = IoTimings {
        shared_read: take("Shared I/O Read Time") + old_read,
        shared_write: take("Shared I/O Write Time") + old_write,
        local_read: take("Local I/O Read Time"),
        local_write: take("Local I/O Write Time"),
        temp_read: take("Temp I/O Read Time"),
        temp_write: take("Temp I/O Write Time"),
    };
    (timings != IoTimings::default()).then_some(timings)
}

fn wal(props: &mut Map<String, Value>) -> Option<Wal> {
    const KEYS: [&str; 4] = ["WAL Records", "WAL FPI", "WAL Bytes", "WAL Buffers Full"];
    if !KEYS.iter().any(|key| props.contains_key(*key)) {
        return None;
    }
    let mut take = |key: &str| take_u64(props, key).unwrap_or(0);
    let wal = Wal {
        records: take("WAL Records"),
        fpi: take("WAL FPI"),
        bytes: take("WAL Bytes"),
        buffers_full: take("WAL Buffers Full"),
    };
    (wal != Wal::default()).then_some(wal)
}

fn workers(props: &mut Map<String, Value>) -> Vec<Worker> {
    let all_objects = matches!(props.get("Workers"), Some(Value::Array(items)) if items.iter().all(Value::is_object));
    if !all_objects {
        return Vec::new();
    }
    let Some(Value::Array(items)) = props.remove("Workers") else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::Object(mut props) => Some(Worker {
                number: take_u64(&mut props, "Worker Number")
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
                actuals: actuals(&mut props),
                buffers: buffers(&mut props),
                io_timings: io_timings(&mut props),
                extra: canonical(props),
            }),
            _ => None,
        })
        .collect()
}

/// PostgreSQL 13 and older list the source of an `INSERT`, `UPDATE` or
/// `DELETE` as a `Member`; later versions as `Outer`.
fn source_of_modifications_is_outer(nodes: &mut [Node]) {
    for index in 0..nodes.len() {
        if nodes[index].node_type != "ModifyTable" {
            continue;
        }
        let sources: Vec<NodeId> = nodes[index]
            .children
            .iter()
            .copied()
            .filter(|&child| {
                matches!(
                    nodes[child.index()].relationship,
                    Some(Relationship::Member | Relationship::Outer)
                )
            })
            .collect();
        if let [source] = sources[..] {
            nodes[source.index()].relationship = Some(Relationship::Outer);
        }
    }
}

fn summary(mut top: Map<String, Value>) -> Summary {
    let mut summary = Summary {
        planning_time: take_f64(&mut top, "Planning Time"),
        execution_time: take_f64(&mut top, "Execution Time"),
        query_identifier: take_i64(&mut top, "Query Identifier"),
        query_text: take_string(&mut top, "Query Text"),
        ..Summary::default()
    };

    if let Some(Value::Object(mut planning)) = take_object(&mut top, "Planning") {
        let section = Planning {
            buffers: buffers(&mut planning),
            io_timings: io_timings(&mut planning),
            memory_used: take_f64(&mut planning, "Memory Used"),
            memory_allocated: take_f64(&mut planning, "Memory Allocated"),
            extra: canonical(planning),
        };
        summary.planning = (section != Planning::default()).then_some(section);
    }

    if matches!(top.get("Triggers"), Some(Value::Array(items)) if items.iter().all(Value::is_object))
    {
        if let Some(Value::Array(items)) = top.remove("Triggers") {
            summary.triggers = items
                .into_iter()
                .filter_map(|item| match item {
                    Value::Object(mut trigger) => Some(Trigger {
                        name: take_string(&mut trigger, "Trigger Name"),
                        constraint: take_string(&mut trigger, "Constraint Name"),
                        relation: take_string(&mut trigger, "Relation"),
                        time: take_f64(&mut trigger, "Time"),
                        calls: take_f64(&mut trigger, "Calls").unwrap_or(0.0),
                        extra: canonical(trigger),
                    }),
                    _ => None,
                })
                .collect();
        }
    }

    if let Some(Value::Object(jit)) = take_object(&mut top, "JIT") {
        summary.jit = Some(jit_section(jit));
    }

    if let Some(Value::Object(settings)) = take_object(&mut top, "Settings") {
        summary.settings = settings
            .into_iter()
            .map(|(name, value)| match value {
                Value::String(text) => (name, text),
                other => (name, other.to_string()),
            })
            .collect();
    }

    if let Some(Value::Object(mut serialization)) = take_object(&mut top, "Serialization") {
        summary.serialization = Some(Serialization {
            time: take_f64(&mut serialization, "Time"),
            output_volume: take_f64(&mut serialization, "Output Volume").unwrap_or(0.0),
            format: take_string(&mut serialization, "Format").unwrap_or_default(),
            buffers: buffers(&mut serialization),
            extra: canonical(serialization),
        });
    }

    summary.extra = canonical(top);
    summary
}

/// The `JIT` object. Unfamiliar keys, including those inside `Options`,
/// `Timing` and `Generation`, are kept in `extra` in the same structure.
fn jit_section(mut jit: Map<String, Value>) -> Jit {
    // PostgreSQL 12 numbers the leader's JIT as worker -1.
    if jit.get("Worker Number").and_then(Value::as_i64) == Some(-1) {
        jit.remove("Worker Number");
    }
    let timing = match take_object(&mut jit, "Timing") {
        Some(Value::Object(mut timing)) => {
            let phases = jit_timing(&mut timing);
            put_back(&mut jit, "Timing", timing);
            Some(phases)
        }
        _ => None,
    };
    let mut options = match take_object(&mut jit, "Options") {
        Some(Value::Object(options)) => options,
        _ => Map::new(),
    };
    let mut option = |key: &str| take_bool(&mut options, key).unwrap_or(false);
    let (inlining, optimization, expressions, deforming) = (
        option("Inlining"),
        option("Optimization"),
        option("Expressions"),
        option("Deforming"),
    );
    put_back(&mut jit, "Options", options);
    Jit {
        functions: take_u64(&mut jit, "Functions").unwrap_or(0),
        inlining,
        optimization,
        expressions,
        deforming,
        timing,
        extra: canonical(jit),
    }
}

fn jit_timing(timing: &mut Map<String, Value>) -> JitTiming {
    // `Generation` is an object with `Deform` and `Total` since PostgreSQL 17.
    let (generation, deform) = match take_object(timing, "Generation") {
        Some(Value::Object(mut generation)) => {
            let parts = (
                take_f64(&mut generation, "Total").unwrap_or(0.0),
                take_f64(&mut generation, "Deform"),
            );
            put_back(timing, "Generation", generation);
            parts
        }
        _ => (take_f64(timing, "Generation").unwrap_or(0.0), None),
    };
    JitTiming {
        generation,
        deform,
        inlining: take_f64(timing, "Inlining").unwrap_or(0.0),
        optimization: take_f64(timing, "Optimization").unwrap_or(0.0),
        emission: take_f64(timing, "Emission").unwrap_or(0.0),
        total: take_f64(timing, "Total").unwrap_or(0.0),
    }
}

/// Returns what remains of a nested object to its parent, if anything does.
fn put_back(parent: &mut Map<String, Value>, key: &str, rest: Map<String, Value>) {
    if !rest.is_empty() {
        parent.insert(key.to_owned(), Value::Object(rest));
    }
}

fn take_object(props: &mut Map<String, Value>, key: &str) -> Option<Value> {
    if props.get(key).is_some_and(Value::is_object) {
        props.remove(key)
    } else {
        None
    }
}

fn take_string(props: &mut Map<String, Value>, key: &str) -> Option<String> {
    if !props.get(key).is_some_and(Value::is_string) {
        return None;
    }
    match props.remove(key) {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}

fn take_f64(props: &mut Map<String, Value>, key: &str) -> Option<f64> {
    let value = props.get(key)?.as_f64()?;
    props.remove(key);
    Some(value)
}

fn take_u64(props: &mut Map<String, Value>, key: &str) -> Option<u64> {
    let value = props.get(key)?.as_f64()?;
    if value < 0.0 || value.fract() != 0.0 || value >= 1.8e19 {
        return None;
    }
    props.remove(key);
    // Checked above: a non-negative integer within range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(value as u64)
}

fn take_i64(props: &mut Map<String, Value>, key: &str) -> Option<i64> {
    let value = props.get(key)?.as_i64()?;
    props.remove(key);
    Some(value)
}

fn take_bool(props: &mut Map<String, Value>, key: &str) -> Option<bool> {
    let value = props.get(key)?.as_bool()?;
    props.remove(key);
    Some(value)
}

fn take_list(props: &mut Map<String, Value>, key: &str) -> Vec<String> {
    let all_strings =
        matches!(props.get(key), Some(Value::Array(items)) if items.iter().all(Value::is_string));
    if !all_strings {
        return Vec::new();
    }
    match props.remove(key) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The remaining properties: zero values the text format omits are dropped
/// and numbers are canonical.
fn canonical(props: Map<String, Value>) -> BTreeMap<String, Value> {
    props
        .into_iter()
        .filter(|(key, value)| {
            !(OMITTED_WHEN_ZERO.contains(&key.as_str()) && value.as_f64() == Some(0.0))
        })
        .map(|(key, mut value)| {
            canonical_numbers(&mut value);
            (key, value)
        })
        .collect()
}

/// Integral floats become integers (`10.00` in PostgreSQL 18 JSON is 10),
/// so that equal values compare equal whichever format they came from.
fn canonical_numbers(value: &mut Value) {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::Number(number) => {
                if let Some(float) = number.as_f64() {
                    if !number.is_i64()
                        && !number.is_u64()
                        && float.fract() == 0.0
                        && float.abs() < 9.0e15
                    {
                        // The range check makes the conversion exact.
                        #[allow(clippy::cast_possible_truncation)]
                        let integer = float as i64;
                        *number = Number::from(integer);
                    }
                }
            }
            Value::Array(items) => stack.extend(items.iter_mut()),
            Value::Object(map) => stack.extend(map.values_mut()),
            _ => {}
        }
    }
}
