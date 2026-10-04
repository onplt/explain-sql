//! Reports for reference plans, as snapshots: a change in the metrics, the
//! rules or the layout shows up as a reviewable diff. Run
//! `cargo insta review` (or set `INSTA_UPDATE=always`) after an intended
//! change.

mod common;

use common::{plan_path, read};
use explainsql_core::{analyze, parse, report};

/// One plan per kind of finding, the traps where nothing must be found, and
/// plans without timing.
const REFERENCE: [&str; 24] = [
    "seq_scan_selective",
    "seq_scan_tiny_table",
    "seq_scan_limit_early_stop",
    "seq_scan_or_across_columns",
    "seq_scan_cast_on_column",
    "misestimate_correlated_columns",
    "misestimate_stale_stats",
    "sort_external_merge",
    "hash_join_batches",
    "hash_aggregate_spill",
    "nested_loop_inner_seq_scan",
    "lateral_join_top_n",
    "index_scan_filter",
    "index_only_scan_heap_fetches",
    "bitmap_lossy",
    "delete_fk_trigger",
    "nested_loop_cartesian",
    "parallel_workers_not_launched",
    "jit_overhead",
    "parallel_hash_join",
    "cte_multiple_scans",
    "initplan",
    "timing_off",
    "estimate_only",
];

#[test]
fn text_reports() {
    for scenario in REFERENCE {
        let plan = parse(&read(&plan_path(16, scenario, "txt"))).unwrap();
        let text = report::text(&plan, &analyze(&plan), false);
        insta::assert_snapshot!(scenario, text);
    }
}

#[test]
fn markdown_report() {
    let plan = parse(&read(&plan_path(16, "lateral_join_top_n", "txt"))).unwrap();
    insta::assert_snapshot!(report::markdown(&plan, &analyze(&plan)));
}

#[test]
fn json_report_holds_everything() {
    let plan = parse(&read(&plan_path(16, "delete_fk_trigger", "json"))).unwrap();
    let analysis = analyze(&plan);
    let value: serde_json::Value = serde_json::from_str(&report::json(&plan, &analysis)).unwrap();
    assert_eq!(value["verdict"], analysis.verdict.as_str());
    assert_eq!(value["findings"][0]["rule"]["id"], "ES009");
    assert_eq!(value["findings"][0]["severity"], "high");
    assert!(
        value["metrics"]["statement"]["trigger_time"]
            .as_f64()
            .unwrap()
            > 300.0
    );
    assert_eq!(value["plan"]["nodes"][0]["node_type"], "ModifyTable");
}

/// Colors only when asked for, and never in the other formats.
#[test]
fn colors_are_optional() {
    let plan = parse(&read(&plan_path(16, "seq_scan_selective", "txt"))).unwrap();
    let analysis = analyze(&plan);
    assert!(report::text(&plan, &analysis, true).contains("\x1b["));
    assert!(!report::text(&plan, &analysis, false).contains('\x1b'));
    assert!(!report::markdown(&plan, &analysis).contains('\x1b'));
}
