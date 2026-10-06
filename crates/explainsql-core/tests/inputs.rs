//! Plans as they arrive in practice: psql output in its various formats,
//! server log entries, copied result cells and pasted text. Every input in
//! `fixtures/inputs` holds the same query, so each must yield the same tree
//! as `reference.txt`.

mod common;

use common::{fixtures, node_differences, read};
use explainsql_core::ir::{Format, Plan, Wrapper};
use explainsql_core::{ParseError, parse, parse_all};

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
    use Wrapper::{
        AutoExplainLog, CsvLog, JsonLog, PsqlExpanded, PsqlTable, PsqlWrapped, QuotedCells,
    };
    let cases: [(&str, Format, &[Wrapper]); 17] = [
        ("reference.json", Format::Json, &[]),
        ("psql-aligned.txt", Format::Text, &[PsqlTable]),
        ("psql-aligned-json.txt", Format::Json, &[PsqlTable]),
        ("psql-unicode.txt", Format::Text, &[PsqlTable]),
        ("psql-unicode-json.txt", Format::Json, &[PsqlTable]),
        ("psql-border2.txt", Format::Text, &[PsqlTable]),
        ("psql-wrapped.txt", Format::Text, &[PsqlTable, PsqlWrapped]),
        ("psql-expanded.txt", Format::Text, &[PsqlExpanded]),
        ("psql-expanded-json.txt", Format::Json, &[PsqlExpanded]),
        // No line of this plan needs quoting, so it reads like unaligned output.
        ("psql-csv.txt", Format::Text, &[PsqlTable]),
        ("psql-csv-json.txt", Format::Json, &[QuotedCells]),
        ("auto_explain-text.log", Format::Text, &[AutoExplainLog]),
        ("auto_explain-json.log", Format::Json, &[AutoExplainLog]),
        ("jsonlog-text.json", Format::Text, &[JsonLog]),
        ("jsonlog-json.json", Format::Json, &[JsonLog]),
        ("csvlog-text.csv", Format::Text, &[CsvLog]),
        ("csvlog-json.csv", Format::Json, &[CsvLog]),
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
        "csvlog-text.csv",
        "csvlog-json.csv",
    ] {
        let plan = parse(&input(name)).unwrap();
        assert_eq!(plan.summary.query_text.as_deref(), Some(query), "{name}");
    }
}

/// A result cell as GUI clients copy it: in double quotes, inner quotes
/// doubled.
fn cell(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[test]
fn pasted_variants_yield_the_reference_tree() {
    let text = input("reference.txt");
    let indented: Vec<String> = text.lines().map(|line| format!("    {line}")).collect();
    let cells: Vec<String> = text.lines().map(cell).collect();
    let some_cells: Vec<String> = text
        .lines()
        .map(|line| {
            if line.contains("->") {
                cell(line)
            } else {
                line.to_owned()
            }
        })
        .collect();
    let variants = [
        (
            "pgAdmin copy with header",
            format!("{}\n{}", cell("QUERY PLAN"), cells.join("\n")),
        ),
        ("cells quoted where needed", some_cells.join("\n")),
        (
            "pgAdmin copy of a JSON plan",
            format!("{}\n{}", cell("QUERY PLAN"), cell(&input("reference.json"))),
        ),
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
fn only_the_first_of_several_plans_is_read() {
    let before = input("reference.txt");
    let after = "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..0.020 rows=1 loops=1)\nPlanning Time: 9.000 ms\nExecution Time: 9.500 ms";

    let plan = parse(&format!("{before}\n\n{after}")).unwrap();
    assert_same_tree("two text plans", &plan);
    assert_eq!(
        plan.summary.execution_time,
        reference().summary.execution_time
    );
    assert_eq!(
        plan.warnings[0].message,
        "the input contains more than one plan; showing the first"
    );

    let json = input("reference.json");
    let plan = parse(&format!("{json}\n{json}")).unwrap();
    assert_same_tree("two JSON plans", &plan);
    assert_eq!(plan.warnings[0].message, "ignored text after the JSON plan");
}

#[test]
fn parse_all_reads_a_single_plan_as_parse_does() {
    for entry in std::fs::read_dir(fixtures().join("inputs")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = read(&path);
        let all = parse_all(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(all, [parse(&text).unwrap()], "{name}");
    }
}

#[test]
fn parse_all_reads_every_plan() {
    let text = input("reference.txt");
    let other = "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..0.020 rows=1 loops=1)\nPlanning Time: 9.000 ms\nExecution Time: 9.500 ms";
    let check = |name: &str, input: &str, wrappers: &[Wrapper]| {
        let plans = parse_all(input).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(plans.len(), 2, "{name}");
        assert_same_tree(name, &plans[0]);
        for plan in &plans {
            assert_eq!(plan.source.wrappers, wrappers, "{name}");
            assert!(plan.warnings.is_empty(), "{name}: {:?}", plan.warnings);
        }
        plans
    };

    // Text plans one after the other: each keeps its own summary.
    let plans = check("two text plans", &format!("{text}\n\n{other}"), &[]);
    assert_eq!(plans[1].root().node_type, "Seq Scan");
    assert_eq!(plans[1].summary.execution_time, Some(9.5));
    assert_eq!(
        plans[0].summary.execution_time,
        reference().summary.execution_time
    );

    // Labels between the plans are left out, and said to be.
    let labelled = format!("Before:\n{text}\n\nAfter:\n{other}");
    let plans = parse_all(&labelled).unwrap();
    assert_eq!(plans.len(), 2);
    assert_same_tree("labelled plans", &plans[0]);
    for plan in &plans {
        let warnings: Vec<&str> = plan.warnings.iter().map(|w| w.message.as_str()).collect();
        assert_eq!(warnings, ["ignored 1 line(s) before the plan"]);
    }
    let first = parse(&labelled).unwrap();
    let warnings: Vec<&str> = first.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(
        warnings,
        [
            "ignored 1 line(s) before the plan",
            "the input contains more than one plan; showing the first"
        ]
    );

    // JSON: two documents, or one array of two plans.
    let json = input("reference.json");
    check("two JSON documents", &format!("{json}\n{json}"), &[]);
    let array = format!(
        "[{}, {}]",
        json.trim().trim_start_matches('[').trim_end_matches(']'),
        json.trim().trim_start_matches('[').trim_end_matches(']')
    );
    check("a JSON array of two plans", &array, &[]);
    // What follows the plans and cannot be read is said to be left out.
    let plans = parse_all(&format!("{json}\n{json}\n{{\"Plan\": ")).unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[1].warnings[0].message,
        "ignored text after the JSON plan"
    );

    // Markdown: every fence, whatever is around them.
    check(
        "two fences",
        &format!("Before:\n\n```\n{text}\n```\n\nAfter:\n\n```sql\n{json}\n```\n"),
        &[Wrapper::MarkdownFence],
    );

    // psql printing two EXPLAINs.
    let table = input("psql-aligned.txt");
    check(
        "two psql tables",
        &format!("db=> EXPLAIN ANALYZE ...;\n{table}\ndb=> EXPLAIN ANALYZE ...;\n{table}\n"),
        &[Wrapper::PsqlTable],
    );

    // Every auto_explain entry of a log, each with its query text.
    for (name, wrapper) in [
        ("auto_explain-text.log", Wrapper::AutoExplainLog),
        ("jsonlog-json.json", Wrapper::JsonLog),
        ("csvlog-text.csv", Wrapper::CsvLog),
    ] {
        let log = input(name);
        let plans = check(
            name,
            &format!("{}\n{}", log.trim_end(), log.trim_end()),
            &[wrapper],
        );
        assert!(
            plans.iter().all(|plan| plan.summary.query_text.is_some()),
            "{name}"
        );
    }
}

#[test]
fn parse_all_needs_one_plan_at_least() {
    assert_eq!(parse_all("  \n"), Err(ParseError::Empty));
    assert_eq!(parse_all("hello world"), Err(ParseError::NoPlan));
    assert!(matches!(
        parse_all("{\"no plan\": 1}"),
        Err(ParseError::InvalidJson(_))
    ));
    // Parts without a plan are skipped.
    let plans = parse_all(&format!(
        "```\nnot a plan\n```\n```\n{}\n```",
        input("reference.txt")
    ))
    .unwrap();
    assert_eq!(plans.len(), 1);
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
