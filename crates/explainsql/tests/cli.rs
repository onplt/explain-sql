//! The `explainsql` binary, run as people run it.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(path)
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_explainsql"))
        .args(args)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.unwrap_or("").as_bytes()).unwrap();
    drop(input);
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn prints_a_report_for_a_file() {
    let path = fixture("pg/16/seq_scan_selective.txt");
    let output = run(&[path.to_str().unwrap()], None);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(
        text.starts_with("11.9 ms. 100% of it in Seq Scan on orders"),
        "{text}"
    );
    assert!(text.contains("ES001 Selective sequential scan"));
    // Not a terminal: no colors.
    assert!(!text.contains('\x1b'));
}

#[test]
fn reads_standard_input_and_other_formats() {
    let plan = std::fs::read_to_string(fixture("inputs/psql-aligned.txt")).unwrap();
    let markdown = stdout(&run(&["--format", "md"], Some(&plan)));
    assert!(markdown.contains("| Share | Time | Node |"), "{markdown}");
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&run(&["--format", "json", "-"], Some(&plan)))).unwrap();
    assert_eq!(json["plan"]["source"]["wrappers"][0], "PsqlTable");
    let colored = stdout(&run(&["--color", "always", "--print"], Some(&plan)));
    assert!(colored.contains("\x1b["));
}

#[test]
fn debug_parse_shows_what_was_read() {
    let plan = std::fs::read_to_string(fixture("inputs/auto_explain-text.log")).unwrap();
    let text = stdout(&run(&["--debug-parse"], Some(&plan)));
    assert!(
        text.starts_with("format: Text (inside AutoExplainLog)"),
        "{text}"
    );
    let json: serde_json::Value = serde_json::from_str(&stdout(&run(
        &["--debug-parse", "--format", "json"],
        Some(&plan),
    )))
    .unwrap();
    assert_eq!(
        json["summary"]["query_text"],
        "SELECT grp, count(*)\nFROM t\nWHERE grp < 10\nGROUP BY grp\nORDER BY grp"
    );
}

#[test]
fn fails_clearly() {
    let output = run(&[], Some("hello"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no EXPLAIN plan"));

    let output = run(&["/nonexistent/plan.json"], None);
    assert_eq!(output.status.code(), Some(1));

    let output = run(&["--frobnicate"], None);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn stops_quietly_when_the_reader_does() {
    // Megabytes of output, far more than a pipe buffers.
    let mut plan = String::from(
        "Append  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..9.000 rows=5000 loops=1)\n",
    );
    for _ in 0..5000 {
        plan.push_str("  ->  Seq Scan on t  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.001 rows=1 loops=1)\n");
    }
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wide-plan.txt");
    std::fs::write(&path, plan).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_explainsql"))
        .args(["--format", "json", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the pipe without reading, as `| head -0` would.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shows_a_demo_plan() {
    let output = run(&["--demo", "--print"], None);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(
        text.contains("ES005 Expensive nested-loop inner side"),
        "{text}"
    );
}

/// As psql's pager with its output going elsewhere than a terminal, it
/// passes everything through, plan or not.
#[test]
fn pager_passes_output_through_when_not_in_a_terminal() {
    let table = " id | name\n----+------\n  1 | x\n(1 row)\n";
    let output = run(&["--pager"], Some(table));
    assert!(output.status.success());
    assert_eq!(stdout(&output), table);
    let plan = std::fs::read_to_string(fixture("inputs/psql-aligned.txt")).unwrap();
    assert_eq!(stdout(&run(&["--pager"], Some(&plan))), plan);
}

#[test]
fn connected_mode_needs_a_query() {
    let output = run(&["-d", "postgresql://localhost/x"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("-f FILE or -c SQL"));
}

/// Against the database named by `EXPLAINSQL_TEST_DATABASE_URL`, as the
/// CI's `db` job provides; skipped without it.
#[test]
fn connected_mode_runs_queries_safely() {
    let Ok(url) = std::env::var("EXPLAINSQL_TEST_DATABASE_URL") else {
        eprintln!("EXPLAINSQL_TEST_DATABASE_URL is not set; skipping");
        return;
    };
    let output = run(
        &[
            "-d",
            &url,
            "-c",
            "SELECT * FROM orders WHERE customer_id = 4242",
        ],
        None,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("CREATE INDEX CONCURRENTLY ON public.orders (customer_id);"),
        "{text}"
    );
    assert!(!text.contains("Not connected"), "{text}");

    let delete = "DELETE FROM orders WHERE id > 199980";
    let refused = run(&["-d", &url, "-c", delete], None);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--allow-dml"));
    let output = run(&["-d", &url, "-c", delete, "--allow-dml"], None);
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("ES009"), "{text}");
    // The foreign key's columns come from the catalog.
    assert!(
        text.contains("CREATE INDEX CONCURRENTLY ON public.order_items (order_id);"),
        "{text}"
    );

    // Testing the suggestion: with HypoPG when installed, else built and
    // rolled back.
    let output = run(
        &[
            "-d",
            &url,
            "-c",
            "SELECT * FROM orders WHERE customer_id = 4242",
            "--prove",
            "--allow-ddl",
        ],
        None,
    );
    let text = stdout(&output);
    assert!(
        text.contains("Estimated with a hypothetical index")
            || text.contains("Measured with the index built and rolled back"),
        "{text}"
    );

    // A query from a file, estimated only.
    let file = std::env::temp_dir().join(format!("explainsql-cli-{}.sql", std::process::id()));
    std::fs::write(&file, "SELECT count(*) FROM orders;\n").unwrap();
    let output = run(
        &[
            "-d",
            &url,
            "-f",
            file.to_str().unwrap(),
            "--no-analyze",
            "--format",
            "json",
        ],
        None,
    );
    std::fs::remove_file(&file).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(json["plan"]["nodes"][0].get("actuals").is_none());
}

/// Asking the planner why, against the database named by
/// `EXPLAINSQL_TEST_DATABASE_URL`; skipped without it.
#[test]
fn connected_mode_asks_the_planner_why() {
    let Ok(url) = std::env::var("EXPLAINSQL_TEST_DATABASE_URL") else {
        eprintln!("EXPLAINSQL_TEST_DATABASE_URL is not set; skipping");
        return;
    };
    let json = |args: &[&str]| -> serde_json::Value {
        let mut all = vec!["-d", url.as_str(), "--format", "json"];
        all.extend_from_slice(args);
        let output = run(&all, None);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_str(&stdout(&output)).unwrap()
    };

    // A function of the column: no index can serve it, and the answer
    // names the index it keeps out.
    let report = json(&[
        "-c",
        "SELECT * FROM orders WHERE date_trunc('day', created_at) = timestamptz '2024-06-01 00:00:00+00'",
        "--why-not",
    ]);
    let answer = &report["counterfactuals"][0];
    assert_eq!(answer["topic"], "index", "{answer}");
    assert_eq!(answer["verdict"], "unusable", "{answer}");
    assert_eq!(answer["settings"][0]["name"], "enable_seqscan");
    assert!(
        answer["evidence"][0]["value"]
            .as_str()
            .unwrap()
            .contains("applies date_trunc() to created_at"),
        "{answer}"
    );

    // Most of the table: the planner can use the index, and estimates it
    // more expensive.
    let broad = "SELECT * FROM orders WHERE created_at < timestamptz '2025-06-01 00:00:00+00'";
    let report = json(&["-c", broad, "--why-not", "orders"]);
    let answer = &report["counterfactuals"][0];
    assert_eq!(answer["verdict"], "costlier", "{answer}");
    assert_eq!(answer["measured"], false);
    // Measured, both plans run the same number of times.
    let report = json(&["-c", broad, "--why-not", "--measure", "--runs", "2"]);
    let answer = &report["counterfactuals"][0];
    assert_eq!(answer["measured"], true, "{answer}");
    assert_eq!(answer["comparison"]["before"]["runs"], 2, "{answer}");
    assert_eq!(answer["comparison"]["after"]["runs"], 2, "{answer}");
    assert_ne!(answer["verdict"], "unusable", "{answer}");

    // A sort that spills: with more work_mem it stays in memory; whether
    // that is faster is what the measurement says.
    let report = json(&[
        "-c",
        "SELECT * FROM orders ORDER BY note",
        "--why-not",
        "--measure",
    ]);
    let answer = &report["counterfactuals"][0];
    assert_eq!(answer["topic"], "memory", "{answer}");
    assert_eq!(answer["comparison"]["after"]["temp_pages"], 0, "{answer}");

    // Nothing to ask about a table the plan does not scan.
    let output = run(
        &[
            "-d",
            &url,
            "-c",
            "SELECT 1",
            "--why-not",
            "orders",
            "--print",
        ],
        None,
    );
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nothing to ask the planner about"),
        "{output:?}"
    );
}

#[test]
fn asking_why_needs_a_database() {
    let path = fixture("pg/16/seq_scan_selective.txt");
    let output = run(&[path.to_str().unwrap(), "--why-not"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--why-not and --measure ask the database")
    );
}

#[test]
fn compares_two_plans() {
    let before = fixture("pg/12/anti_join.json");
    let after = fixture("pg/18/anti_join.txt");
    let (before, after) = (before.to_str().unwrap(), after.to_str().unwrap());
    let output = run(&["diff", before, after], None);
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("Worse: pages 2,478 → 2,623"), "{text}");
    assert!(text.contains(
        "JOIN      Merge Anti Join of customers c and orders o became Hash Right Anti Join"
    ));
    assert!(text.contains("The plan after"));
    assert!(text.contains("Only in the plan before: Sort."));
    assert!(!text.contains('\x1b'));

    // For a pull request, or for another program.
    let markdown = stdout(&run(&["diff", before, after, "--format", "md"], None));
    assert!(
        markdown.contains("- **Join:** Merge Anti Join"),
        "{markdown}"
    );
    let json: serde_json::Value = serde_json::from_str(&stdout(&run(
        &["diff", before, after, "--format", "json"],
        None,
    )))
    .unwrap();
    assert_eq!(json["changes"][0]["kind"], "join");
    assert_eq!(json["labels"]["after"][0], "Hash Right Anti Join");
    assert_ne!(json["shapes"]["before"], json["shapes"]["after"]);
}

#[test]
fn compares_two_plans_in_one_input() {
    let first = std::fs::read_to_string(fixture("pg/12/anti_join.txt")).unwrap();
    let second = std::fs::read_to_string(fixture("pg/18/anti_join.txt")).unwrap();
    let pasted = format!("Before:\n{first}\n\nAfter:\n{second}");
    let output = run(&["diff", "-"], Some(&pasted));
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("became Hash Right Anti Join"));
    // The labels were left out, and said to be.
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(
        errors.contains("the plan after: ignored 1 line(s) before the plan"),
        "{errors}"
    );

    // One plan is not enough.
    let output = run(&["diff", "-"], Some(&first));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("the input holds one plan"));
    // Nor is text that is not a plan.
    let path = fixture("pg/12/anti_join.txt");
    let output = run(&["diff", "-", path.to_str().unwrap()], Some("hello"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("the plan before"));
    // Usage errors are told apart.
    assert_eq!(run(&["diff"], None).status.code(), Some(2));
}
