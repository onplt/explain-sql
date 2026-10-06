//! Reports for reference plans, as snapshots: a change in the metrics, the
//! rules or the layout shows up as a reviewable diff. Run
//! `cargo insta review` (or set `INSTA_UPDATE=always`) after an intended
//! change.

mod common;

use common::{plan_path, read};
use explainsql_core::counterfactual::{self, Evaluation, Target};
use explainsql_core::ir::Plan;
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

/// What the database said when asked why: a function of the column keeps
/// the index out, and the planner estimates an index on a selective filter
/// about as expensive as the scan.
fn asked(scenario: &str, alternative: &str) -> (Plan, explainsql_core::Analysis) {
    let plan = parse(&read(&plan_path(16, scenario, "txt"))).unwrap();
    let mut analysis = analyze(&plan);
    let question =
        counterfactual::questions(&plan, &analysis, None, &Target::Hotspots, false).remove(0);
    let alternative = parse(alternative).unwrap();
    let answer = counterfactual::answer(
        &plan,
        &analysis,
        &question,
        &Evaluation {
            chosen: &[],
            alternative: std::slice::from_ref(&alternative),
            with_cost_settings: None,
            cost_settings_runs: &[],
            catalog: None,
        },
    );
    analysis.record(vec![answer]);
    (plan, analysis)
}

#[test]
fn why_not_reports() {
    let (plan, analysis) = asked(
        "seq_scan_function_on_column",
        "Seq Scan on public.orders  (cost=10000000000.00..10000005417.00 rows=1000 width=64)\n  Filter: (date_trunc('day'::text, orders.created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)",
    );
    let text = report::text(&plan, &analysis, false);
    let section = &text[text.find("Why not").unwrap()..text.find("Share and Time:").unwrap()];
    insta::assert_snapshot!("why_not_text", section);

    let (plan, analysis) = asked(
        "seq_scan_selective",
        "Bitmap Heap Scan on public.orders  (cost=12.00..5210.00 rows=10 width=64)\n  Recheck Cond: (orders.customer_id = 4242)\n  ->  Bitmap Index Scan on orders_customer_id_idx  (cost=0.00..12.00 rows=10 width=0)\n        Index Cond: (orders.customer_id = 4242)",
    );
    let markdown = report::markdown(&plan, &analysis);
    insta::assert_snapshot!(
        "why_not_markdown",
        &markdown[markdown.find("### Why not").unwrap()..]
    );
    let value: serde_json::Value = serde_json::from_str(&report::json(&plan, &analysis)).unwrap();
    let answer = &value["counterfactuals"][0];
    assert_eq!(answer["verdict"], "costlier");
    assert_eq!(answer["topic"], "index");
    assert_eq!(answer["relation"], "orders");
    assert_eq!(answer["comparison"]["basis"], "cost");
    // Not asked: no section, and nothing in the JSON.
    let plan = parse(&read(&plan_path(16, "seq_scan_selective", "txt"))).unwrap();
    let analysis = analyze(&plan);
    assert!(!report::text(&plan, &analysis, false).contains("Why not"));
    let value: serde_json::Value = serde_json::from_str(&report::json(&plan, &analysis)).unwrap();
    assert!(value.get("counterfactuals").is_none());
}

/// A diff in each format: the anti-join that PostgreSQL 18 plans another way.
#[test]
fn diff_reports() {
    let before = parse(&read(&plan_path(12, "anti_join", "json"))).unwrap();
    let after = parse(&read(&plan_path(18, "anti_join", "json"))).unwrap();
    let diff = explainsql_core::diff::diff(&before, &after);
    insta::assert_snapshot!(
        "diff_text",
        report::diff_text(&before, &after, &diff, false)
    );
    insta::assert_snapshot!(
        "diff_markdown",
        report::diff_markdown(&before, &after, &diff)
    );
    let json: serde_json::Value =
        serde_json::from_str(&report::diff_json(&before, &after, &diff)).unwrap();
    assert_eq!(
        json["changes"].as_array().unwrap().len(),
        diff.changes.len()
    );
    assert_eq!(json["matched"][0], serde_json::json!([0, 0]));
}

/// The checks of a CI run: a plan worse than its locked one, with its
/// tested fix, a new plan and a plan that passed.
#[test]
fn check_reports() {
    use explainsql_core::check::{Policy, check};
    let indexed = "Index Scan using orders_customer_id_idx on orders o  (cost=0.42..44.50 rows=10 width=20) (actual time=0.020..0.051 rows=10 loops=1)\n  Index Cond: (customer_id = 4242)\n  Buffers: shared hit=13\nExecution Time: 0.070 ms";
    let scanned = read(&plan_path(16, "seq_scan_selective", "txt"));
    let (indexed, scanned) = (parse(indexed).unwrap(), parse(&scanned).unwrap());
    let checked = vec![
        check(
            "queries/customer.sql",
            scanned.clone(),
            analyze(&scanned),
            Some(indexed.clone()),
            Policy::default(),
        ),
        check(
            "queries/new.sql",
            indexed.clone(),
            analyze(&indexed),
            None,
            Policy::default(),
        ),
        check(
            "queries/same.sql",
            indexed.clone(),
            analyze(&indexed),
            Some(indexed),
            Policy::default(),
        ),
    ];
    insta::assert_snapshot!("check_text", report::check_text(&checked, false));
    insta::assert_snapshot!("check_markdown", report::check_markdown(&checked));
    let sarif: serde_json::Value = serde_json::from_str(&report::check_sarif(&checked)).unwrap();
    assert_eq!(
        sarif["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap()
            .len(),
        15
    );
}

/// How the plan of a statement with parameters depends on their values: a
/// customer's latest orders, whose generic plan walks the whole index of
/// dates for a customer with few orders.
#[test]
fn parameters_reports() {
    use explainsql_core::params::{
        self, ColumnStats, Trial, Tried, hold, row_counts, samples, sensitivity, trials,
    };
    let sql = "SELECT * FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2";
    let generic = parse(
        "\
Limit  (cost=0.42..1597.22 rows=1 width=64)
  ->  Index Scan Backward using orders_created_at_idx on orders  (cost=0.42..15968.46 rows=10 width=64)
        Filter: (customer_id = $1)",
    )
    .unwrap();
    let sorted = parse(
        "\
Limit  (cost=5094.32..5095.01 rows=6 width=64)
  ->  Gather Merge  (cost=5094.32..5095.01 rows=6 width=64)
        Workers Planned: 1
        ->  Sort  (cost=4094.31..4094.33 rows=6 width=64)
              Sort Key: created_at DESC
              ->  Parallel Seq Scan on orders  (cost=0.00..4094.24 rows=6 width=64)
                    Filter: (customer_id = 4242)",
    )
    .unwrap();
    let sorted_run = parse(
        "\
Limit  (cost=5094.32..5095.01 rows=6 width=64) (actual time=9.736..12.595 rows=10 loops=1)
  Buffers: shared hit=2667
  ->  Gather Merge  (cost=5094.32..5095.01 rows=6 width=64) (actual time=9.735..12.592 rows=10 loops=1)
        Workers Planned: 1
        Workers Launched: 1
        Buffers: shared hit=2667
        ->  Sort  (cost=4094.31..4094.33 rows=6 width=64) (actual time=7.161..7.162 rows=5 loops=2)
              Sort Key: created_at DESC
              Sort Method: quicksort  Memory: 25kB
              Buffers: shared hit=2667
              ->  Parallel Seq Scan on orders  (cost=0.00..4094.24 rows=6 width=64) (actual time=1.153..7.105 rows=5 loops=2)
                    Filter: (customer_id = 4242)
                    Rows Removed by Filter: 99995
                    Buffers: shared hit=2610
Planning Time: 0.131 ms
Execution Time: 12.635 ms",
    )
    .unwrap();
    let generic_run = parse(
        "\
Limit  (cost=0.42..1597.22 rows=1 width=64) (actual time=2.730..67.100 rows=10 loops=1)
  Buffers: shared hit=176778
  ->  Index Scan Backward using orders_created_at_idx on orders  (cost=0.42..15968.46 rows=10 width=64) (actual time=2.728..67.088 rows=10 loops=1)
        Filter: (customer_id = $1)
        Rows Removed by Filter: 164404
        Buffers: shared hit=176778
Planning Time: 0.200 ms
Execution Time: 67.123 ms",
    )
    .unwrap();

    let types = ["integer".to_owned(), "bigint".to_owned()];
    let mut parameters = params::parameters(&generic, sql, &types);
    let stats = ColumnStats {
        table: "orders".to_owned(),
        n_distinct: 19_897.0,
        common_values: vec!["15453".to_owned()],
        common_freqs: vec![0.000_27],
        histogram: (0..101).map(|i| (1 + i * 200).to_string()).collect(),
        ..ColumnStats::default()
    };
    let samples = vec![samples(&stats, "="), row_counts(params::Clause::Limit)];
    hold(&mut parameters, &samples, &[]);
    assert_eq!(trials(&parameters, &samples).len(), 2 + 5);
    let trial = |parameter: usize, value: &str, values: [&str; 2]| Trial {
        parameter: Some(parameter),
        sample: samples[parameter - 1]
            .iter()
            .find(|sample| sample.value.as_deref() == Some(value))
            .cloned(),
        values: values.map(|value| Some(value.to_owned())).to_vec(),
    };
    let tried = vec![
        Tried {
            trial: trial(1, "15453", ["15453", "10"]),
            custom: generic.clone(),
            generic: generic.clone(),
            measured: None,
            timed_out: None,
        },
        Tried {
            trial: trial(1, "10001", ["10001", "10"]),
            custom: sorted.clone(),
            generic: generic.clone(),
            measured: Some(explainsql_core::compare::compare(&sorted_run, &generic_run)),
            timed_out: None,
        },
        Tried {
            trial: trial(2, "1", ["15453", "1"]),
            custom: generic.clone(),
            generic: generic.clone(),
            measured: None,
            timed_out: None,
        },
    ];
    let mut sensitivity = sensitivity(&generic, parameters, tried, true);
    let worst = sensitivity.worst().unwrap();
    let values = sensitivity.rows[worst].values.clone();
    sensitivity.show_measured(&values, true);
    let mut analysis = analyze(&generic_run);
    analysis.parameters = Some(sensitivity);
    insta::assert_snapshot!(
        "parameters_text",
        report::text(&generic_run, &analysis, false)
    );
    insta::assert_snapshot!(
        "parameters_markdown",
        report::markdown(&generic_run, &analysis)
    );
    let json: serde_json::Value =
        serde_json::from_str(&report::json(&generic_run, &analysis)).unwrap();
    assert_eq!(json["parameters"]["verdict"], "sensitive");
    assert_eq!(json["parameters"]["parameters"][1]["clause"], "limit");
    assert_eq!(json["parameters"]["rows"][1]["measured"]["change"], "worse");
}

/// The plans of a log over time: an index dropped, and a prepared statement
/// that switched to its generic plan.
#[test]
fn logs_reports() {
    let text = read(&common::fixtures().join("logs/postgresql.log"));
    let (entries, _) = explainsql_core::parse_log(&text).unwrap();
    let timeline = explainsql_core::timeline::timeline(&entries);
    insta::assert_snapshot!("logs_text", report::logs_text(&entries, &timeline, false));
    insta::assert_snapshot!("logs_markdown", report::logs_markdown(&entries, &timeline));
    let json: serde_json::Value =
        serde_json::from_str(&report::logs_json(&entries, &timeline)).unwrap();
    assert_eq!(json["statements"].as_array().unwrap().len(), 3);
    assert_eq!(json["log"].as_array().unwrap().len(), 32);
    assert_eq!(json["log"][0]["trace"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(json["log"][0]["line"], 6);
}
