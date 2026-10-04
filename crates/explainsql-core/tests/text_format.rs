//! Text-format constructs that the fixture corpus does not cover: other
//! node types, quoted names, and properties only some plans print.

use explainsql_core::ir::{Node, Plan, PredicateKind, Relationship};
use explainsql_core::parse;

fn plan(text: &str) -> Plan {
    let plan = parse(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    plan
}

/// The root of a one-node plan built from a node line.
fn node(header: &str) -> Node {
    plan(&format!("{header}  (cost=0.00..1.00 rows=1 width=4)"))
        .root()
        .clone()
}

#[test]
fn node_names() {
    let join = node("Parallel Hash Right Anti Join");
    assert_eq!(
        (
            join.node_type.as_str(),
            join.join_type.as_deref(),
            join.parallel_aware
        ),
        ("Hash Join", Some("Right Anti"), true)
    );
    let join = node("Nested Loop Left Join");
    assert_eq!(
        (join.node_type.as_str(), join.join_type.as_deref()),
        ("Nested Loop", Some("Left"))
    );
    let join = node("Nested Loop");
    assert_eq!(join.join_type.as_deref(), Some("Inner"));

    let aggregate = node("Finalize GroupAggregate");
    assert_eq!(
        (
            aggregate.node_type.as_str(),
            aggregate.strategy.as_deref(),
            aggregate.partial_mode.as_deref()
        ),
        ("Aggregate", Some("Sorted"), Some("Finalize"))
    );
    assert_eq!(node("MixedAggregate").strategy.as_deref(), Some("Mixed"));
    assert_eq!(node("Aggregate").partial_mode.as_deref(), Some("Simple"));

    let setop = node("HashSetOp Except All");
    assert_eq!(
        (
            setop.node_type.as_str(),
            setop.strategy.as_deref(),
            setop.command.as_deref()
        ),
        ("SetOp", Some("Hashed"), Some("Except All"))
    );

    let merge = node("Merge on public.t");
    assert_eq!(
        (merge.node_type.as_str(), merge.operation.as_deref()),
        ("ModifyTable", Some("Merge"))
    );
    let foreign = node("Foreign Update on public.remote");
    assert_eq!(
        (foreign.node_type.as_str(), foreign.operation.as_deref()),
        ("Foreign Scan", Some("Update"))
    );
    let foreign = node("Async Foreign Scan on public.remote r");
    assert!(foreign.async_capable);
    assert_eq!(foreign.alias.as_deref(), Some("r"));
    let custom = node("Custom Scan (ChunkAppend) on metrics");
    assert_eq!(
        (
            custom.node_type.as_str(),
            custom.custom_plan_provider.as_deref(),
            custom.relation_name.as_deref()
        ),
        ("Custom Scan", Some("ChunkAppend"), Some("metrics"))
    );
}

#[test]
fn targets_and_quoted_names() {
    let scan = node(r#"Index Only Scan Backward using "My Idx" on "Sales"."Order" "o""1""#);
    assert_eq!(scan.node_type, "Index Only Scan");
    assert_eq!(scan.scan_direction.as_deref(), Some("Backward"));
    assert_eq!(scan.index_name.as_deref(), Some("My Idx"));
    assert_eq!(scan.schema.as_deref(), Some("Sales"));
    assert_eq!(scan.relation_name.as_deref(), Some("Order"));
    assert_eq!(scan.alias.as_deref(), Some("o\"1"));

    let values = node(r#"Values Scan on "*VALUES*""#);
    assert_eq!(
        (values.relation_name.as_deref(), values.alias.as_deref()),
        (None, Some("*VALUES*"))
    );
    let subquery = node("Subquery Scan on sub");
    assert_eq!(
        (subquery.relation_name.as_deref(), subquery.alias.as_deref()),
        (None, Some("sub"))
    );
    let function = node("Function Scan on pg_catalog.generate_series g");
    assert_eq!(function.function_name.as_deref(), Some("generate_series"));
    assert_eq!(function.schema.as_deref(), Some("pg_catalog"));
    assert_eq!(function.alias.as_deref(), Some("g"));
    // The alias is printed only when it differs from the name.
    assert_eq!(node("Seq Scan on orders").alias.as_deref(), Some("orders"));
}

#[test]
fn measurements_in_node_lines() {
    let never = plan("Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (never executed)");
    let actuals = never.root().actuals.unwrap();
    assert!(actuals.never_executed());
    assert_eq!(actuals.total_time, None);

    let untimed =
        plan("Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (actual rows=2.50 loops=4)");
    let actuals = untimed.root().actuals.unwrap();
    assert_eq!(
        (actuals.rows, actuals.loops, actuals.total_time),
        (2.5, 4, None)
    );

    let costless = plan("Seq Scan on t\n  Filter: (a = 1)");
    assert_eq!(costless.root().estimates, None);
    assert_eq!(
        costless.root().predicate(PredicateKind::Filter),
        Some("(a = 1)")
    );
}

#[test]
fn compound_properties() {
    let plan = plan(
        "\
Incremental Sort  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
  Output: f(a, b), 'x, y'::text, c
  Pre-sorted Groups: 2  Sort Methods: top-N heapsort, quicksort  Average Memory: 26kB  Peak Memory: 27kB  Average Disk: 1kB  Peak Disk: 2kB
  Buffers: shared hit=1 read=2 dirtied=3 written=4, local hit=5, temp read=6 written=7
  I/O Timings: shared read=1.5 write=0.5, local read=0.25, temp write=2.0
  WAL: records=10 fpi=2 bytes=1200 buffers full=1
  Worker 0:  Sort Method: quicksort  Memory: 25kB
  ->  Hash  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
        Buckets: 4096 (originally 1024)  Batches: 4 (originally 1)  Memory Usage: 89kB",
    );
    let root = plan.root();
    assert_eq!(root.output, ["f(a, b)", "'x, y'::text", "c"]);
    assert_eq!(
        root.extra["Pre-sorted Groups"],
        serde_json::json!({
            "Group Count": 2,
            "Sort Methods Used": ["top-N heapsort", "quicksort"],
            "Sort Space Memory": {"Average Sort Space Used": 26, "Peak Sort Space Used": 27},
            "Sort Space Disk": {"Average Sort Space Used": 1, "Peak Sort Space Used": 2},
        })
    );
    let buffers = root.buffers.unwrap();
    assert_eq!(
        (
            buffers.shared_dirtied,
            buffers.local_hit,
            buffers.temp_read,
            buffers.temp_written
        ),
        (3, 5, 6, 7)
    );
    let io = root.io_timings.unwrap();
    assert_eq!(
        (io.shared_read, io.local_read, io.temp_write),
        (1.5, 0.25, 2.0)
    );
    let wal = root.wal.unwrap();
    assert_eq!(
        (wal.records, wal.fpi, wal.bytes, wal.buffers_full),
        (10, 2, 1200, 1)
    );
    assert_eq!(root.workers[0].extra["Sort Method"], "quicksort");
    assert_eq!(root.workers[0].extra["Sort Space Used"], 25);

    let hash = plan.node(root.children[0]);
    assert_eq!(hash.extra["Hash Buckets"], 4096);
    assert_eq!(hash.extra["Original Hash Buckets"], 1024);
    assert_eq!(hash.extra["Original Hash Batches"], 1);
    assert_eq!(hash.extra["Peak Memory Usage"], 89);
}

#[test]
fn statement_summary() {
    let plan = plan(
        "\
Insert on public.t  (cost=0.00..1.00 rows=0 width=0) (actual time=0.1..0.2 rows=0 loops=1)
  ->  Result  (cost=0.00..0.01 rows=1 width=4) (actual time=0.01..0.02 rows=1 loops=1)
Settings: search_path = '\"$user\", public', work_mem = 'it''s 1MB'
Query Identifier: -1234567890123
Planning Time: 0.100 ms
Serialization: output=10kB  format=binary
Trigger for constraint t_fk: time=1.500 calls=2
Trigger audit on t: calls=3
Execution Time: 3.000 ms",
    );
    let summary = &plan.summary;
    assert_eq!(summary.settings["search_path"], "\"$user\", public");
    assert_eq!(summary.settings["work_mem"], "it's 1MB");
    assert_eq!(summary.query_identifier, Some(-1_234_567_890_123));
    let serialization = summary.serialization.as_ref().unwrap();
    assert_eq!(
        (
            serialization.time,
            serialization.output_volume,
            serialization.format.as_str()
        ),
        (None, 10.0, "binary")
    );
    let first = &summary.triggers[0];
    assert_eq!(
        (
            first.name.as_deref(),
            first.constraint.as_deref(),
            first.time,
            first.calls
        ),
        (None, Some("t_fk"), Some(1.5), 2.0)
    );
    let second = &summary.triggers[1];
    assert_eq!(
        (
            second.name.as_deref(),
            second.relation.as_deref(),
            second.time
        ),
        (Some("audit"), Some("t"), None)
    );
    assert_eq!(summary.execution_time, Some(3.0));
}

#[test]
fn subplan_children() {
    let plan = plan(
        "\
Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)
  Filter: (a > (InitPlan 1).col1)
  InitPlan 1
    ->  Result  (cost=0.00..0.01 rows=1 width=4)
  SubPlan 2
    ->  Index Scan using u_pkey on u  (cost=0.00..1.00 rows=1 width=4)
          Index Cond: (u.id = t.id)",
    );
    let initplan = plan.node(plan.root().children[0]);
    assert_eq!(
        (initplan.relationship, initplan.subplan_name.as_deref()),
        (Some(Relationship::InitPlan), Some("InitPlan 1"))
    );
    let subplan = plan.node(plan.root().children[1]);
    assert_eq!(
        (subplan.relationship, subplan.subplan_name.as_deref()),
        (Some(Relationship::SubPlan), Some("SubPlan 2"))
    );
    assert_eq!(
        subplan.predicate(PredicateKind::IndexCond),
        Some("(u.id = t.id)")
    );
}

#[test]
fn unfamiliar_lines_are_kept_with_a_warning() {
    let plan = parse(
        "\
Motion 3:1  (slice1; segments: 3)  (cost=0.00..1.00 rows=1 width=4)
  Frobnication: 42
  something odd",
    )
    .unwrap();
    let root = plan.root();
    assert_eq!(root.node_type, "Motion 3:1  (slice1; segments: 3)");
    assert_eq!(root.extra["Frobnication"], 42);
    assert_eq!(
        root.extra["Unparsed Lines"],
        serde_json::json!(["something odd"])
    );
    assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
}
