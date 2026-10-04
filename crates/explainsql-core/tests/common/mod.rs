//! Helpers shared by the integration tests: the fixture corpus and plan
//! comparison.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use explainsql_core::ir::{Node, Plan};
use serde_json::Value;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every captured plan pair: (major version, scenario).
pub fn corpus() -> Vec<(u32, String)> {
    let mut pairs = Vec::new();
    for entry in fs::read_dir(fixtures().join("pg")).unwrap() {
        let dir = entry.unwrap().path();
        let major: u32 = dir.file_name().unwrap().to_str().unwrap().parse().unwrap();
        let manifest: Value = serde_json::from_str(&read(&dir.join("manifest.json"))).unwrap();
        for (name, scenario) in manifest["scenarios"].as_object().unwrap() {
            if scenario["status"] == "generated" {
                pairs.push((major, name.clone()));
            }
        }
    }
    pairs.sort();
    pairs
}

pub fn plan_path(major: u32, scenario: &str, extension: &str) -> PathBuf {
    fixtures()
        .join("pg")
        .join(major.to_string())
        .join(format!("{scenario}.{extension}"))
}

/// Properties of parallel nodes that differ between two executions, because
/// how much of the work the leader does varies.
const VOLATILE_IN_PARALLEL: [&str; 2] = ["Peak Memory Usage", "Sort Space Used"];

/// Differences between two plans of the same query in what does not change
/// between executions: tree shape, node identity, estimates, row counts and
/// deterministic properties. Times, buffers and per-worker details are
/// ignored.
pub fn node_differences(a: &Plan, b: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    if a.nodes.len() != b.nodes.len() {
        out.push(format!(
            "{} nodes vs {}: {:?} vs {:?}",
            a.nodes.len(),
            b.nodes.len(),
            types(a),
            types(b)
        ));
        return out;
    }
    // A rolled-back data modification still grows the table, and the
    // planner scales its estimates by the table's current size, so the
    // estimates of such plans drift from one capture to the next.
    let modifies_data = a.root().node_type == "ModifyTable";
    for (x, y) in a.nodes.iter().zip(&b.nodes) {
        let parallel = runs_in_parallel(a, x);
        node_pair(x, y, modifies_data, parallel, &mut out);
    }
    out
}

/// Whether the node is below a Gather or Gather Merge.
fn runs_in_parallel(plan: &Plan, node: &Node) -> bool {
    let mut parent = node.parent;
    while let Some(id) = parent {
        let ancestor = plan.node(id);
        if ancestor.node_type.starts_with("Gather") {
            return true;
        }
        parent = ancestor.parent;
    }
    false
}

fn types(plan: &Plan) -> Vec<&str> {
    plan.nodes
        .iter()
        .map(|node| node.node_type.as_str())
        .collect()
}

fn node_pair(x: &Node, y: &Node, modifies_data: bool, parallel: bool, out: &mut Vec<String>) {
    let mut check = |field: &str, left: String, right: String| {
        if left != right {
            out.push(format!(
                "node {} ({}): {field}: {left} vs {right}",
                x.id.0, x.node_type
            ));
        }
    };
    macro_rules! same {
        ($($field:ident),* $(,)?) => {
            $(check(stringify!($field), format!("{:?}", x.$field), format!("{:?}", y.$field));)*
        };
    }
    same!(
        parent,
        children,
        node_type,
        relationship,
        subplan_name,
        parallel_aware,
        async_capable,
        disabled,
        inner_unique,
        single_copy,
        join_type,
        strategy,
        partial_mode,
        operation,
        command,
        scan_direction,
        schema,
        relation_name,
        alias,
        index_name,
        cte_name,
        function_name,
        custom_plan_provider,
        predicates,
        output,
        sort_key,
        presorted_key,
        group_key,
        rows_removed_by_filter,
        rows_removed_by_join_filter,
        rows_removed_by_index_recheck,
        workers_planned,
        workers_launched,
    );
    if !modifies_data {
        check(
            "estimates",
            format!("{:?}", x.estimates),
            format!("{:?}", y.estimates),
        );
    }
    let rows = |node: &Node| node.actuals.map(|actuals| (actuals.rows, actuals.loops));
    check(
        "actual rows and loops",
        format!("{:?}", rows(x)),
        format!("{:?}", rows(y)),
    );
    let timed = |node: &Node| node.actuals.map(|actuals| actuals.total_time.is_some());
    check(
        "timed",
        format!("{:?}", timed(x)),
        format!("{:?}", timed(y)),
    );
    check(
        "extra",
        format!("{:?}", stable(&x.extra, parallel)),
        format!("{:?}", stable(&y.extra, parallel)),
    );
}

fn stable(extra: &BTreeMap<String, Value>, parallel: bool) -> BTreeMap<&String, &Value> {
    extra
        .iter()
        .filter(|(key, _)| !(parallel && VOLATILE_IN_PARALLEL.contains(&key.as_str())))
        .collect()
}

/// Differences in the statement-level parts that do not change between
/// executions.
pub fn summary_differences(a: &Plan, b: &Plan) -> Vec<String> {
    let mut out = Vec::new();
    let (x, y) = (&a.summary, &b.summary);
    let mut check = |field: &str, left: String, right: String| {
        if left != right {
            out.push(format!("summary {field}: {left} vs {right}"));
        }
    };
    check(
        "settings",
        format!("{:?}", x.settings),
        format!("{:?}", y.settings),
    );
    let triggers = |plan: &Plan| {
        plan.summary
            .triggers
            .iter()
            .map(|t| (t.name.clone(), t.constraint.clone(), t.calls))
            .collect::<Vec<_>>()
    };
    check(
        "triggers",
        format!("{:?}", triggers(a)),
        format!("{:?}", triggers(b)),
    );
    let jit = |plan: &Plan| {
        plan.summary.jit.as_ref().map(|jit| {
            (
                jit.functions,
                jit.inlining,
                jit.optimization,
                jit.expressions,
                jit.deforming,
            )
        })
    };
    check("jit", format!("{:?}", jit(a)), format!("{:?}", jit(b)));
    check(
        "planning present",
        format!("{:?}", x.planning.is_some()),
        format!("{:?}", y.planning.is_some()),
    );
    let memory = |plan: &Plan| {
        plan.summary
            .planning
            .as_ref()
            .map(|p| (p.memory_used.is_some(), p.memory_allocated.is_some()))
    };
    check(
        "planning memory",
        format!("{:?}", memory(a)),
        format!("{:?}", memory(b)),
    );
    let serialization = |plan: &Plan| {
        plan.summary
            .serialization
            .as_ref()
            .map(|s| (s.output_volume, s.format.clone()))
    };
    check(
        "serialization",
        format!("{:?}", serialization(a)),
        format!("{:?}", serialization(b)),
    );
    check(
        "times present",
        format!(
            "{:?}",
            (x.planning_time.is_some(), x.execution_time.is_some())
        ),
        format!(
            "{:?}",
            (y.planning_time.is_some(), y.execution_time.is_some())
        ),
    );
    check(
        "query identifier",
        format!("{:?}", x.query_identifier),
        format!("{:?}", y.query_identifier),
    );
    check("extra", format!("{:?}", x.extra), format!("{:?}", y.extra));
    out
}
