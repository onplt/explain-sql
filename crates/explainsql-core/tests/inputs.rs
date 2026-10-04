//! Plans as they arrive in practice: psql output in its various formats,
//! server log entries and pasted text. Every input in `fixtures/inputs` holds
//! the same query, so each must yield the same tree as `reference.txt`.

mod common;

use common::{fixtures, node_differences, read};
use explainsql_core::ir::{Format, Plan, Wrapper};
use explainsql_core::parse;

fn input(name: &str) -> String {
    read(&fixtures().join("inputs").join(name))
}

fn reference() -> Plan {
    parse(&input("reference.txt")).expect("the reference parses")
}

fn assert_same_tree(name: &str, plan: &Plan) {
    let differences = node_differences(&reference(), plan);
    assert!(differences.is_empty(), "{name}: {differences:#?}");
}

#[test]
fn every_captured_input_yields_the_reference_tree() {
    use Wrapper::{AutoExplainLog, JsonLog, PsqlExpanded, PsqlTable, PsqlWrapped};
    let cases: [(&str, Format, &[Wrapper]); 13] = [
        ("reference.json", Format::Json, &[]),
        ("psql-aligned.txt", Format::Text, &[PsqlTable]),
        ("psql-aligned-json.txt", Format::Json, &[PsqlTable]),
        ("psql-unicode.txt", Format::Text, &[PsqlTable]),
        ("psql-unicode-json.txt", Format::Json, &[PsqlTable]),
        ("psql-border2.txt", Format::Text, &[PsqlTable]),
        ("psql-wrapped.txt", Format::Text, &[PsqlTable, PsqlWrapped]),
        ("psql-expanded.txt", Format::Text, &[PsqlExpanded]),
        ("psql-expanded-json.txt", Format::Json, &[PsqlExpanded]),
        ("auto_explain-text.log", Format::Text, &[AutoExplainLog]),
        ("auto_explain-json.log", Format::Json, &[AutoExplainLog]),
        ("jsonlog-text.json", Format::Text, &[JsonLog]),
        ("jsonlog-json.json", Format::Json, &[JsonLog]),
    ];
    for (name, format, wrappers) in cases {
        let plan = parse(&input(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(plan.source.format, format, "{name}");
        assert_eq!(plan.source.wrappers, wrappers, "{name}");
        assert!(plan.warnings.is_empty(), "{name}: {:?}", plan.warnings);
        assert_same_tree(name, &plan);
    }
}

#[test]
fn log_entries_keep_the_query_text() {
    let query = "SELECT grp, count(*)\nFROM t\nWHERE grp < 10\nGROUP BY grp\nORDER BY grp";
    for name in [
        "auto_explain-text.log",
        "auto_explain-json.log",
        "jsonlog-text.json",
        "jsonlog-json.json",
    ] {
        let plan = parse(&input(name)).unwrap();
        assert_eq!(plan.summary.query_text.as_deref(), Some(query), "{name}");
    }
}

#[test]
fn pasted_variants_yield_the_reference_tree() {
    let text = input("reference.txt");
    let indented: Vec<String> = text.lines().map(|line| format!("    {line}")).collect();
    let variants = [
        (
            "Markdown fence",
            format!("Here is the plan:\n\n```text\n{text}\n```\n\nThanks!"),
        ),
        ("CRLF line endings", text.replace('\n', "\r\n")),
        (
            "prompt and unaligned psql header",
            format!(
                "db=> EXPLAIN (ANALYZE, BUFFERS) SELECT grp, count(*) ...;\nQUERY PLAN\n{text}\n(9 rows)\n"
            ),
        ),
        ("shared indentation", indented.join("\n")),
        (
            "fenced psql output",
            format!("```\n{}\n```", input("psql-aligned.txt")),
        ),
        ("non-breaking spaces", text.replace("  ", "\u{a0} ")),
    ];
    for (name, variant) in variants {
        let plan = parse(&variant).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(plan.warnings.is_empty(), "{name}: {:?}", plan.warnings);
        assert_same_tree(name, &plan);
    }
}

#[test]
fn truncated_plans_still_parse() {
    // A text plan cut in the middle of a node: the part before it remains.
    let text = input("reference.txt");
    let cut = text.find("Index Cond").unwrap();
    let plan = parse(&text[..cut]).unwrap();
    assert_eq!(plan.nodes.len(), 2);

    // A JSON plan cut in the middle of a string.
    let json = input("reference.json");
    let cut = json.find("\"Index Cond\"").unwrap() + 5;
    let plan = parse(&json[..cut]).unwrap();
    assert_eq!(plan.nodes.len(), 2);
    assert_eq!(plan.warnings.len(), 1);
}

#[test]
fn what_is_not_a_plan_is_rejected() {
    for input in [
        "",
        "   \n\n",
        "hello world",
        "SELECT * FROM t;",
        "[1, 2, 3]",
        "{\"a\": 1}",
    ] {
        assert!(parse(input).is_err(), "{input:?} was accepted");
    }
}
