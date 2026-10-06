//! `anonymize` against every plan in `fixtures/pg` and every input form in
//! `fixtures/inputs`: the plans read back the same, and nothing they named
//! is left.

mod common;

use std::collections::BTreeSet;

use common::{corpus, fixtures, node_differences, plan_path, read, summary_differences};
use explainsql_core::anonymize::{Options, anonymize};
use explainsql_core::ir::Plan;
use explainsql_core::{analyze, parse, parse_all};

/// What a plan names: relations, aliases, indexes, CTEs, schemas, the
/// columns of qualified references, constraints and string literals.
fn names(plan: &Plan) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut texts = Vec::new();
    for (_, node) in plan.walk() {
        for name in [
            &node.relation_name,
            &node.alias,
            &node.index_name,
            &node.cte_name,
            &node.schema,
        ]
        .into_iter()
        .flatten()
        {
            names.insert(name.clone());
        }
        texts.extend(node.output.iter().cloned());
        texts.extend(node.sort_key.iter().cloned());
        texts.extend(node.group_key.iter().cloned());
        texts.extend(node.predicates.iter().map(|p| p.text.clone()));
    }
    for trigger in &plan.summary.triggers {
        names.extend(trigger.constraint.clone());
    }
    for text in texts {
        // String literals, whole.
        for (index, part) in text.split('\'').enumerate() {
            if index % 2 == 1 && part.len() > 2 && part.parse::<f64>().is_err() {
                names.insert(part.to_owned());
            }
        }
        // The column of each qualified reference: `o.customer_id`.
        let words: Vec<&str> = text
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .collect();
        for word in words {
            if let Some((qualifier, column)) = word.rsplit_once('.') {
                let is_name = |s: &str| s.starts_with(|c: char| c.is_alphabetic() || c == '_');
                if is_name(qualifier) && is_name(column) && column != "col1" {
                    names.insert(column.to_owned());
                }
            }
        }
    }
    for kept in [
        "public",
        "pg_catalog",
        "ctid",
        "*VALUES*",
        "generate_series",
        "col1",
        "col2",
    ] {
        names.remove(kept);
    }
    names
}

/// The names of `original` still found as whole words in `text`.
fn leaks(original: &Plan, text: &str) -> Vec<String> {
    let words: BTreeSet<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .collect();
    names(original)
        .into_iter()
        .filter(|name| {
            if name.contains(|c: char| !(c.is_alphanumeric() || c == '_')) {
                text.contains(name.as_str())
            } else {
                words.contains(name.as_str())
            }
        })
        .collect()
}

fn rules(plan: &Plan) -> Vec<String> {
    analyze(plan)
        .findings
        .iter()
        .map(|finding| format!("{:?}", finding.rule))
        .collect()
}

#[test]
fn every_plan_reads_back_the_same_without_its_names() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        for extension in ["json", "txt"] {
            let label = format!("PostgreSQL {major} {scenario}.{extension}");
            let input = read(&plan_path(major, &scenario, extension));
            let original = parse(&input).unwrap();
            let anonymized = match anonymize(&input, Options::default()) {
                Ok(anonymized) => anonymized,
                Err(error) => {
                    problems.push(format!("{label}: {error}"));
                    continue;
                }
            };
            let plan = parse(&anonymized.text).unwrap();
            if !plan.warnings.is_empty() {
                problems.push(format!("{label}: warnings {:?}", plan.warnings));
            }
            let leaked = leaks(&original, &anonymized.text);
            if !leaked.is_empty() {
                problems.push(format!("{label}: still names {leaked:?}"));
            }
            let types = |plan: &Plan| -> Vec<String> {
                plan.nodes.iter().map(|n| n.node_type.clone()).collect()
            };
            let figures = |plan: &Plan| -> Vec<String> {
                plan.nodes
                    .iter()
                    .map(|n| format!("{:?} {:?} {:?}", n.estimates, n.actuals, n.buffers))
                    .collect()
            };
            if types(&plan) != types(&original) || figures(&plan) != figures(&original) {
                problems.push(format!("{label}: the nodes changed"));
            }
            if rules(&plan) != rules(&original) {
                problems.push(format!(
                    "{label}: findings {:?} became {:?}",
                    rules(&original),
                    rules(&plan)
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

/// The same names in the same order whatever the format: the anonymized
/// JSON and text of a plan still lower to the same plan.
#[test]
fn json_and_text_anonymize_alike() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        let [json, text] = ["json", "txt"].map(|extension| {
            let input = read(&plan_path(major, &scenario, extension));
            parse(&anonymize(&input, Options::default()).unwrap().text).unwrap()
        });
        for difference in node_differences(&json, &text)
            .into_iter()
            .chain(summary_differences(&json, &text))
        {
            problems.push(format!("PostgreSQL {major} {scenario}: {difference}"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} differences:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// psql's output, server logs and the like: the plans come out bare, each
/// one anonymized.
#[test]
fn every_input_form_comes_out_as_its_plans() {
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(fixtures().join("inputs")).unwrap() {
        let path = entry.unwrap().path();
        let input = read(&path);
        let label = path.file_name().unwrap().to_string_lossy().into_owned();
        let originals = parse_all(&input).unwrap();
        match anonymize(&input, Options::default()) {
            Ok(anonymized) => {
                let plans = parse_all(&anonymized.text).unwrap();
                if plans.len() != originals.len() {
                    problems.push(format!(
                        "{label}: {} plans became {}",
                        originals.len(),
                        plans.len()
                    ));
                }
                for original in &originals {
                    let leaked = leaks(original, &anonymized.text);
                    if !leaked.is_empty() {
                        problems.push(format!("{label}: still names {leaked:?}"));
                    }
                }
            }
            Err(error) => problems.push(format!("{label}: {error}")),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn keeping_names_changes_only_values() {
    let input = read(&plan_path(16, "seq_scan_selective", "txt"));
    let anonymized = anonymize(&input, Options { keep_names: true }).unwrap();
    assert!(anonymized.mapping.tables.is_empty());
    assert!(anonymized.mapping.columns.is_empty());
    assert!(!anonymized.mapping.values.is_empty());
    let (original, plan) = (parse(&input).unwrap(), parse(&anonymized.text).unwrap());
    assert_eq!(plan.root().relation_name, original.root().relation_name);
    assert_ne!(plan.root().predicates, original.root().predicates);
}

/// Guards the check itself: the names of a plan are found in its own text.
#[test]
fn the_leak_check_finds_names() {
    let input = read(&plan_path(16, "hash_join", "txt"));
    let leaked = leaks(&parse(&input).unwrap(), &input);
    assert!(leaked.contains(&"orders".to_owned()), "{leaked:?}");
    assert!(leaked.contains(&"customer_id".to_owned()), "{leaked:?}");
}
