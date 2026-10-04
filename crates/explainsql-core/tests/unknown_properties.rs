//! Properties the parsers do not know, such as those a newer server version
//! adds, are kept under their own names in the `extra` map of the node,
//! worker or section they belong to.

use explainsql_core::parse;
use serde_json::json;

#[test]
fn unknown_json_properties_are_kept() {
    let input = json!([{
        "Plan": {
            "Node Type": "Gather",
            "Total Cost": 1.0,
            "Frobnication": 42,
            "Plans": [{
                "Node Type": "Seq Scan",
                "Parent Relationship": "Outer",
                "Relation Name": "t",
                "Pruning": {"Mode": "eager"},
                "Workers": [{"Worker Number": 0, "Actual Loops": 1, "Actual Rows": 1, "Spindle": "left"}]
            }]
        },
        "Planning": {"Shared Hit Blocks": 3, "Cache Warmth": "high"},
        "Triggers": [{"Trigger Name": "audit", "Relation": "t", "Time": 1.5, "Calls": 2, "Depth": 1}],
        "JIT": {
            "Functions": 3,
            "Options": {"Inlining": false, "Optimization": false, "Expressions": true, "Deforming": true, "Vectorize": true},
            "Timing": {
                "Generation": {"Deform": 0.1, "Total": 0.5, "Emit Calls": 4},
                "Inlining": 0.0, "Optimization": 0.2, "Emission": 1.0, "Total": 1.7, "Linking": 0.3
            },
            "Cache": "cold"
        },
        "Serialization": {"Time": 0.5, "Output Volume": 10, "Format": "text", "Compression": "none"},
        "Execution Time": 3.0,
        "Statement Kind": "SELECT"
    }]);
    let plan = parse(&input.to_string()).unwrap();
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

    let gather = plan.root();
    assert_eq!(gather.extra["Frobnication"], 42);
    let scan = plan.node(gather.children[0]);
    assert_eq!(scan.extra["Pruning"], json!({"Mode": "eager"}));
    assert_eq!(scan.workers[0].extra["Spindle"], "left");

    let summary = &plan.summary;
    assert_eq!(summary.extra["Statement Kind"], "SELECT");
    assert_eq!(
        summary.planning.as_ref().unwrap().extra["Cache Warmth"],
        "high"
    );
    assert_eq!(summary.triggers[0].extra["Depth"], 1);
    assert_eq!(
        summary.serialization.as_ref().unwrap().extra["Compression"],
        "none"
    );
    let jit = summary.jit.as_ref().unwrap();
    assert_eq!(jit.timing.unwrap().deform, Some(0.1));
    assert_eq!(jit.extra["Cache"], "cold");
    assert_eq!(jit.extra["Options"], json!({"Vectorize": true}));
    assert_eq!(
        jit.extra["Timing"],
        json!({"Generation": {"Emit Calls": 4}, "Linking": 0.3})
    );
}

/// Known keys holding values of an unexpected type are not dropped either.
#[test]
fn json_values_of_an_unexpected_type_are_kept() {
    let input = json!({
        "Plan": {"Node Type": "Result", "Actual Loops": 1, "Actual Rows": "many", "Workers": "none"},
        "Triggers": [{"Trigger Name": "audit", "Calls": "two"}],
        "JIT": {"Functions": 3, "Options": "default", "Timing": {"Generation": "fast", "Total": 1.0}},
        "Planning Time": "quick"
    });
    let plan = parse(&input.to_string()).unwrap();
    let root = plan.root();
    assert_eq!(root.extra["Actual Rows"], "many");
    assert_eq!(root.extra["Workers"], "none");
    let summary = &plan.summary;
    assert_eq!(summary.extra["Planning Time"], "quick");
    assert_eq!(summary.triggers[0].extra["Calls"], "two");
    let jit = summary.jit.as_ref().unwrap();
    assert_eq!(jit.extra["Options"], "default");
    assert_eq!(jit.extra["Timing"], json!({"Generation": "fast"}));
}

#[test]
fn unknown_text_lines_are_kept() {
    let plan = parse(
        "\
Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..0.020 rows=1 loops=1)
  Frobnication: 42
  Worker 0:  actual time=0.100..0.200 rows=1 loops=1
    Spindle: left
Planning:
  Buffers: shared hit=3
  Cache Warmth: high
Planning Time: 0.100 ms
JIT:
  Functions: 3
  Options: Inlining false, Optimization false, Expressions true, Deforming true
  Timing: Generation 0.500 ms (Deform 0.100 ms), Inlining 0.000 ms, Optimization 0.200 ms, Emission 1.000 ms, Total 1.700 ms
  Cache: cold
Serialization: time=0.500 ms  output=10kB  format=text
  Compression: none
Statement Kind: SELECT
Execution Time: 3.000 ms",
    )
    .unwrap();
    let root = plan.root();
    assert_eq!(root.extra["Frobnication"], 42);
    assert_eq!(root.workers[0].extra["Spindle"], "left");

    let summary = &plan.summary;
    assert_eq!(
        summary.planning.as_ref().unwrap().extra["Cache Warmth"],
        "high"
    );
    assert_eq!(summary.jit.as_ref().unwrap().extra["Cache"], "cold");
    assert_eq!(
        summary.serialization.as_ref().unwrap().extra["Compression"],
        "none"
    );
    // A statement line without a known label is kept verbatim.
    assert_eq!(
        summary.extra["Unparsed Lines"],
        json!(["Statement Kind: SELECT"])
    );
    assert_eq!(summary.execution_time, Some(3.0));

    // One warning per unfamiliar line.
    let lines: Vec<_> = plan.warnings.iter().map(|warning| warning.line).collect();
    assert_eq!(
        lines,
        [Some(2), Some(4), Some(7), Some(13), Some(15), Some(16)]
    );
}
