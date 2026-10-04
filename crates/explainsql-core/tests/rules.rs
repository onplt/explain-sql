//! The rules against every plan in the corpus. Each scenario's header lists
//! the rules its plan triggers, and no other rule may fire; a rule marked
//! with `?` may fire on some server versions and not on others. Scenarios
//! with no rules, such as the traps for naive advisors, check that the rules
//! stay silent.

mod common;

use std::collections::BTreeSet;

use common::{corpus, fixtures, plan_path, read};
use explainsql_core::rules::{self, Finding, RULES};
use explainsql_core::{metrics, parse};

/// The rules a scenario requires, and those it allows.
fn expectations(scenario: &str) -> (Vec<String>, Vec<String>) {
    let source = read(&fixtures().join("scenarios").join(format!("{scenario}.sql")));
    let list = source
        .lines()
        .take_while(|line| line.starts_with("--"))
        .find_map(|line| line.strip_prefix("-- rules:"))
        .unwrap_or("");
    let (mut required, mut optional) = (Vec::new(), Vec::new());
    for rule in list
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
    {
        match rule.strip_suffix('?') {
            Some(id) => optional.push(id.to_owned()),
            None => required.push(rule.to_owned()),
        }
    }
    (required, optional)
}

fn findings(major: u32, scenario: &str, extension: &str) -> Vec<Finding> {
    let plan = parse(&read(&plan_path(major, scenario, extension))).unwrap();
    let metrics = metrics::compute(&plan);
    rules::check(&plan, &metrics)
}

#[test]
fn every_plan_triggers_exactly_its_rules() {
    let mut problems = Vec::new();
    let mut fired_anywhere = BTreeSet::new();
    let mut optional_fired = BTreeSet::new();
    let mut optional_declared = BTreeSet::new();
    for (major, scenario) in corpus() {
        let (required, optional) = expectations(&scenario);
        for id in &optional {
            optional_declared.insert((scenario.clone(), id.clone()));
        }
        for extension in ["json", "txt"] {
            let label = format!("PostgreSQL {major} {scenario}.{extension}");
            let fired: BTreeSet<&str> = findings(major, &scenario, extension)
                .iter()
                .map(|finding| finding.rule.id)
                .collect();
            for id in &required {
                if !fired.contains(id.as_str()) {
                    problems.push(format!("{label}: {id} did not fire"));
                }
            }
            for &id in &fired {
                fired_anywhere.insert(id);
                if optional.iter().any(|optional| optional == id) {
                    optional_fired.insert((scenario.clone(), id.to_owned()));
                } else if !required.iter().any(|required| required == id) {
                    problems.push(format!("{label}: {id} fired unexpectedly"));
                }
            }
        }
    }
    for (scenario, id) in optional_declared.difference(&optional_fired) {
        problems.push(format!(
            "{scenario}: {id}? never fires; drop it from the header"
        ));
    }
    for rule in RULES {
        if !fired_anywhere.contains(rule.id) {
            problems.push(format!("{} never fires in the corpus", rule.id));
        }
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The text that tells people what to do, on a few representative plans.
#[test]
fn findings_explain_themselves() {
    let cases = [
        (
            "seq_scan_selective",
            "ES001",
            "An index on orders (customer_id)",
        ),
        (
            "seq_scan_cast_on_column",
            "ES001",
            "applies a cast to customer_id",
        ),
        (
            "seq_scan_function_on_column",
            "ES001",
            "applies date_trunc() to created_at",
        ),
        ("seq_scan_like_substring", "ES001", "pg_trgm"),
        ("seq_scan_like_prefix", "ES001", "text_pattern_ops"),
        (
            "nested_loop_inner_seq_scan",
            "ES001",
            "An index on order_items (order_id)",
        ),
        (
            "nested_loop_inner_seq_scan",
            "ES005",
            "An index on order_items (order_id)",
        ),
        (
            "lateral_join_top_n",
            "ES005",
            "An index on orders (customer_id)",
        ),
        ("lateral_join_top_n", "ES006", "starts with customer_id"),
        ("index_scan_filter", "ES006", "orders (status, created_at)"),
        (
            "misestimate_correlated_columns",
            "ES002",
            "CREATE STATISTICS ON city, country FROM addresses",
        ),
        (
            "delete_fk_trigger",
            "ES009",
            "Index the referencing columns",
        ),
        (
            "hash_join_batches",
            "ES004",
            "the whole table takes roughly 11.1 MB",
        ),
        (
            "seq_scan_cast_on_column",
            "ES002",
            "no statistics for an expression of customer_id",
        ),
        (
            "initplan",
            "ES002",
            "a value known only when the query runs",
        ),
        ("jit_overhead", "ES012", "jit_inline_above_cost"),
    ];
    for (scenario, id, phrase) in cases {
        for extension in ["json", "txt"] {
            let found = findings(16, scenario, extension);
            let finding = found
                .iter()
                .find(|finding| finding.rule.id == id)
                .unwrap_or_else(|| panic!("{scenario}.{extension}: no {id}"));
            let text = format!("{} {}", finding.summary, finding.action);
            assert!(text.contains(phrase), "{scenario}.{extension} {id}: {text}");
        }
    }
}

#[test]
fn severity_follows_the_share_of_the_runtime() {
    let found = findings(16, "seq_scan_selective", "json");
    assert_eq!(found.len(), 1);
    // The scan is the whole query.
    assert_eq!(found[0].severity, rules::Severity::High);
    // A trigger that takes nearly all of a DELETE's time.
    let found = findings(16, "delete_fk_trigger", "txt");
    assert_eq!(found[0].severity, rules::Severity::High);
    assert_eq!(found[0].node, None);
}
