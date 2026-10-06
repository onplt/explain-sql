//! The parsers against every plan in `fixtures/pg`.

mod common;

use common::{corpus, node_differences, plan_path, read, summary_differences};
use explainsql_core::ir::Format;
use explainsql_core::{parse, parse_all};

#[test]
fn every_plan_parses_without_warnings() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        for (extension, format) in [("json", Format::Json), ("txt", Format::Text)] {
            let label = format!("PostgreSQL {major} {scenario}.{extension}");
            match parse(&read(&plan_path(major, &scenario, extension))) {
                Ok(plan) => {
                    if plan.source.format != format {
                        problems.push(format!("{label}: read as {:?}", plan.source.format));
                    }
                    for warning in &plan.warnings {
                        problems.push(format!(
                            "{label}: line {:?}: {}",
                            warning.line, warning.message
                        ));
                    }
                }
                Err(error) => problems.push(format!("{label}: {error}")),
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
fn parse_all_finds_the_one_plan_of_each_file() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        for extension in ["json", "txt"] {
            let text = read(&plan_path(major, &scenario, extension));
            let all = parse_all(&text).map_err(|e| e.to_string());
            if all != Ok(vec![parse(&text).unwrap()]) {
                problems.push(format!("PostgreSQL {major} {scenario}.{extension}"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn json_and_text_have_the_same_shape() {
    let mut problems = Vec::new();
    for (major, scenario) in corpus() {
        let json = parse(&read(&plan_path(major, &scenario, "json"))).unwrap();
        let text = parse(&read(&plan_path(major, &scenario, "txt"))).unwrap();
        let (json_shape, text_shape) = (
            explainsql_core::fingerprint::shape(&json),
            explainsql_core::fingerprint::shape(&text),
        );
        if json_shape != text_shape {
            problems.push(format!(
                "PostgreSQL {major} {scenario}:\n--- json\n{json_shape}--- text\n{text_shape}"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} differ:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn json_and_text_lower_to_the_same_plan() {
    let mut problems = Vec::new();
    let pairs = corpus();
    for (major, scenario) in &pairs {
        let (Ok(json), Ok(text)) = (
            parse(&read(&plan_path(*major, scenario, "json"))),
            parse(&read(&plan_path(*major, scenario, "txt"))),
        ) else {
            problems.push(format!("PostgreSQL {major} {scenario}: does not parse"));
            continue;
        };
        for difference in node_differences(&json, &text)
            .into_iter()
            .chain(summary_differences(&json, &text))
        {
            problems.push(format!("PostgreSQL {major} {scenario}: {difference}"));
        }
    }
    assert!(pairs.len() > 400, "only {} plan pairs found", pairs.len());
    assert!(
        problems.is_empty(),
        "{} differences:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Guards the comparison itself: a changed value must not go unnoticed.
#[test]
fn the_comparison_notices_differences() {
    let original = read(&plan_path(16, "index_only_scan_heap_fetches", "txt"));
    let json = parse(&read(&plan_path(
        16,
        "index_only_scan_heap_fetches",
        "json",
    )))
    .unwrap();
    assert!(node_differences(&json, &parse(&original).unwrap()).is_empty());
    for (from, to) in [
        ("Heap Fetches: ", "Heap Fetches: 1"),
        ("rows=2000 ", "rows=2001 "),
        ("(page_views.page >= 10)", "(page_views.page >= 11)"),
        ("Index Only Scan using", "Index Only Scan Backward using"),
        ("Output: page", "Output: page, 1"),
    ] {
        let changed = original.replacen(from, to, 1);
        assert_ne!(changed, original, "{from} is not in the plan");
        let text = parse(&changed).unwrap();
        assert!(
            !node_differences(&json, &text).is_empty(),
            "{from} → {to} went unnoticed"
        );
    }
}
