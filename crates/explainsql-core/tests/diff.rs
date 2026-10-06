//! Plan diffs against the corpus: a plan matches itself node for node, the
//! JSON and text forms of a plan do not differ, and plans of a scenario from
//! different PostgreSQL versions compare without surprises.

mod common;

use common::{corpus, plan_path, read};
use explainsql_core::diff::diff;
use explainsql_core::ir::Plan;
use explainsql_core::parse;

fn plan(major: u32, scenario: &str, extension: &str) -> Plan {
    parse(&read(&plan_path(major, scenario, extension))).unwrap()
}

#[test]
fn a_plan_does_not_differ_from_itself_or_its_other_format() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        let json = plan(major, &scenario, "json");
        let text = plan(major, &scenario, "txt");
        for (label, before, after) in [("itself", &json, &json), ("text", &json, &text)] {
            let compared = diff(before, after);
            let all_matched = compared.matched.len() == before.nodes.len()
                && compared.removed.is_empty()
                && compared.added.is_empty()
                && compared.matched.iter().all(|&(a, b)| a == b);
            // The two formats come from two runs: times differ, nothing else.
            let unexpected = compared.changes.iter().any(|change| {
                label == "itself" || change.kind != explainsql_core::diff::ChangeKind::Work
            });
            if !compared.shapes.same() || unexpected || !all_matched {
                problems.push(format!(
                    "PostgreSQL {major} {scenario} against {label}: {} change(s), removed {:?}, added {:?}: {:?}",
                    compared.changes.len(),
                    compared.removed,
                    compared.added,
                    compared
                        .changes
                        .iter()
                        .map(|change| change.summary.as_str())
                        .collect::<Vec<_>>()
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn every_scan_finds_its_relation_in_another_version() {
    let pairs = corpus();
    let mut problems = Vec::new();
    for (major, scenario) in &pairs {
        // The same scenario on the next version that has it.
        let Some((other, _)) = pairs
            .iter()
            .find(|(next, name)| next > major && name == scenario)
        else {
            continue;
        };
        let before = plan(*major, scenario, "json");
        let after = plan(*other, scenario, "json");
        let compared = diff(&before, &after);
        // Every relation read in both plans is matched.
        for &(a, b) in &compared.matched {
            let (a, b) = (before.node(a), after.node(b));
            let both_scans = a.node_type.ends_with("Scan") && b.node_type.ends_with("Scan");
            if both_scans && a.relation_name != b.relation_name {
                problems.push(format!(
                    "{scenario} {major}→{other}: {} matched {}",
                    explainsql_core::format::node(a),
                    explainsql_core::format::node(b)
                ));
            }
        }
        // Each change has a sentence and a weight between 0 and 1.
        for change in &compared.changes {
            if change.summary.is_empty() || !(0.0..=1.0).contains(&change.weight) {
                problems.push(format!("{scenario} {major}→{other}: {change:?}"));
            }
        }
        assert!(!compared.verdict.is_empty());
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn any_two_plans_compare_without_failing() {
    // Plans of different statements share nothing, which must still work.
    let pairs = corpus();
    let plans: Vec<Plan> = pairs
        .iter()
        .filter(|(major, _)| *major == 16)
        .map(|(major, scenario)| plan(*major, scenario, "json"))
        .collect();
    for (before, after) in plans.iter().zip(plans.iter().skip(1)) {
        let compared = diff(before, after);
        assert!(!compared.verdict.is_empty());
        assert_eq!(
            compared.matched.len() + compared.removed.len(),
            before.nodes.len()
        );
        assert_eq!(
            compared.matched.len() + compared.added.len(),
            after.nodes.len()
        );
    }
}

/// Summaries of a diff's changes, with their kinds.
fn changes(before: &Plan, after: &Plan) -> Vec<String> {
    diff(before, after)
        .changes
        .iter()
        .map(|change| format!("{} {}", change.kind.label(), change.summary))
        .collect()
}

#[test]
fn what_changed_from_postgresql_12_to_18() {
    // The planner reads customers in full and hashes it, rather than
    // merging with its primary key.
    assert_eq!(
        changes(
            &plan(12, "anti_join", "json"),
            &plan(18, "anti_join", "json")
        ),
        [
            "JOIN Merge Anti Join of customers c and orders o became Hash Right Anti Join",
            "ACCESS Index Only Scan using customers_pkey on customers c became Seq Scan on customers c",
            "REMOVED Sort removed",
            "WORK Seq Scan on orders o: time 15.3 ms → 23.8 ms (1.6× slower) for the same pages and disk reads",
        ]
    );
    // The hash join swapped its sides.
    assert_eq!(
        changes(
            &plan(12, "cte_materialized", "json"),
            &plan(18, "cte_materialized", "json")
        )[0],
        "JOIN Hash Join of customers c and totals t now probes the hash table with customers c"
    );
    // Bitmap scans for three partitions, told once. The scans in the
    // InitPlan and in the Append read the same partition, and each finds
    // its own counterpart, although PostgreSQL 18 renamed the partitions.
    assert_eq!(
        changes(
            &plan(12, "partition_prune_runtime", "json"),
            &plan(18, "partition_prune_runtime", "json")
        ),
        ["ACCESS Index Only Scan became Bitmap Heap Scan on 3 partitions of events_2025_0*"]
    );
    // Partitions named another way are still the same plan.
    let renamed = diff(
        &plan(12, "partition_append_all", "json"),
        &plan(18, "partition_append_all", "json"),
    );
    assert!(renamed.shapes.same());
    assert!(renamed.removed.is_empty() && renamed.added.is_empty());
}
