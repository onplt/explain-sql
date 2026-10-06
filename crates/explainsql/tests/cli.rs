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

/// The costliest statements from pg_stat_statements, printed, against the
/// database named by `EXPLAINSQL_TEST_DATABASE_URL`; skipped without it or
/// without the extension.
#[test]
fn lists_the_costliest_statements() {
    let Ok(url) = std::env::var("EXPLAINSQL_TEST_DATABASE_URL") else {
        eprintln!("EXPLAINSQL_TEST_DATABASE_URL is not set; skipping");
        return;
    };
    // All of them: other tests' statements may take more time.
    let output = run(
        &["top", "-d", &url, "--limit", "1000", "--format", "json"],
        None,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("pg_stat_statements is not installed") {
        eprintln!("pg_stat_statements is not installed; skipping");
        return;
    }
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(
        json["source"].as_str().unwrap().contains("PostgreSQL "),
        "{json}"
    );
    let statements = json["statements"].as_array().unwrap();
    assert!(!statements.is_empty() && statements.len() <= 1000, "{json}");
    let totals: Vec<f64> = statements
        .iter()
        .map(|statement| statement["total_ms"].as_f64().unwrap())
        .collect();
    assert!(
        totals.windows(2).all(|pair| pair[0] >= pair[1]),
        "{totals:?}"
    );
    // explainsql's own EXPLAINs are counted, and have no plan of their own.
    let explains = statements
        .iter()
        .find(|statement| statement["query"].as_str().unwrap().starts_with("EXPLAIN"))
        .unwrap_or_else(|| panic!("{json}"));
    assert!(
        explains["unplannable"]
            .as_str()
            .unwrap()
            .contains("without a plan")
    );

    let output = run(
        &["top", "-d", &url, "--limit", "5", "--color", "never"],
        None,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("The 5 costliest statements in "), "{text}");
    let output = run(&["top", "-d", &url, "--limit", "5", "--format", "md"], None);
    assert!(
        stdout(&output).starts_with("### The costliest statements in "),
        "{output:?}"
    );
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
            .contains("--why-not, --params and --measure ask the database")
    );
}

/// Statements with parameters, against the database named by
/// `EXPLAINSQL_TEST_DATABASE_URL`; skipped without it.
#[test]
fn connected_mode_tries_the_values_of_parameters() {
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

    // A customer's latest orders, as a Java application sends them: the
    // generic plan walks the index of dates and filters, which suits the
    // LIMIT it cannot see but not a customer with few orders.
    let latest = "SELECT * FROM orders WHERE customer_id = ? ORDER BY created_at DESC LIMIT ?";
    let report = json(&["-c", latest, "--params", "--measure"]);
    let parameters = &report["parameters"];
    assert_eq!(parameters["verdict"], "sensitive", "{parameters}");
    assert_eq!(parameters["converted"], true);
    assert_eq!(
        parameters["parameters"][0]["column"]["column"],
        "customer_id"
    );
    assert_eq!(parameters["parameters"][1]["clause"], "limit");
    let worst = parameters["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["measured"]["change"] == "worse")
        .unwrap_or_else(|| panic!("{parameters}"));
    assert_eq!(worst["generic"], false);
    assert!(
        worst["measured"]["after"]["pages"].as_u64().unwrap()
            > 10 * worst["measured"]["before"]["pages"].as_u64().unwrap(),
        "{worst}"
    );
    assert!(
        parameters["advice"][0]
            .as_str()
            .unwrap()
            .contains("force_custom_plan"),
        "{parameters}"
    );
    // The plan shown is the generic plan, measured with those values, and
    // its advice is the index that serves both.
    assert!(
        parameters["shown"]
            .as_str()
            .unwrap()
            .contains("the values it does worst with")
    );
    assert!(report["plan"]["summary"]["execution_time"].is_number());
    assert!(
        report["advice"][0]["ddl"]
            .as_str()
            .unwrap_or_default()
            .contains("(customer_id, created_at)"),
        "{}",
        report["advice"]
    );

    // No index on status: every value gets the same plan.
    let report = json(&["-c", "SELECT * FROM orders WHERE status = $1", "--params"]);
    assert_eq!(report["parameters"]["verdict"], "insensitive");
    assert_eq!(report["parameters"]["rows"][0]["value"], "delivered");

    // Values given: those alone. A value the statistics cannot give needs
    // one.
    let expression = "SELECT * FROM orders WHERE lower(note) = $1";
    let report = json(&["-c", expression, "--params"]);
    assert_eq!(report["parameters"]["verdict"], "unknown");
    let report = json(&["-c", expression, "--bind", "1=abc"]);
    let rows = report["parameters"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["values"][0], "abc");

    // A statement with parameters needs --params, and --params needs one.
    let output = run(
        &[
            "-d",
            &url,
            "-c",
            "SELECT * FROM orders WHERE id = $1",
            "--print",
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--params tries values for it"),
        "{output:?}"
    );
    let output = run(&["-d", &url, "-c", "SELECT 1", "--params", "--print"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("takes no parameters"));
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

const INDEXED: &str = "\
Index Scan using orders_customer_id_idx on orders o  (cost=0.42..44.50 rows=10 width=20) (actual time=0.020..0.051 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=13
Execution Time: 0.070 ms
";

const SCANNED: &str = "\
Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.899 ms
";

/// A directory of its own for a test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("explainsql-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("plans")).unwrap();
    dir
}

#[test]
fn checks_plans_against_their_locked_plans() {
    let dir = scratch("check");
    let plans = dir.join("plans");
    let lock = dir.join("explainsql.lock");
    let (plans_arg, lock_arg) = (plans.to_str().unwrap(), lock.to_str().unwrap());
    std::fs::write(plans.join("customer.txt"), INDEXED).unwrap();
    let check = |extra: &[&str]| {
        let mut args = vec!["check", plans_arg, "--lock", lock_arg, "--color", "never"];
        args.extend_from_slice(extra);
        run(&args, None)
    };

    // Nothing locked yet: new, and passing.
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).starts_with("NEW   plans/customer.txt"));

    // Locked, then the same plan passes.
    assert_eq!(check(&["--update"]).status.code(), Some(0));
    let locked = std::fs::read_to_string(&lock).unwrap();
    assert!(locked.contains("\"plans/customer.txt\""), "{locked}");
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("PASS  plans/customer.txt"));

    // The index is gone: worse by pages, which fails.
    std::fs::write(plans.join("customer.txt"), SCANNED).unwrap();
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("FAIL  plans/customer.txt"), "{text}");
    assert!(
        text.contains("Worse than the locked plan: pages 13 → 2,417 (186× more)"),
        "{text}"
    );
    assert!(text.ends_with("1 plan: 1 failed.\n"), "{text}");

    // For code scanning and for a pull request.
    let sarif: serde_json::Value =
        serde_json::from_str(&stdout(&check(&["--format", "sarif"]))).unwrap();
    assert_eq!(sarif["version"], "2.1.0");
    let results = sarif["runs"][0]["results"].as_array().unwrap();
    assert!(results.iter().any(|result| result["ruleId"] == "plan-worse"
        && result["level"] == "error"
        && result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
            == "plans/customer.txt"));
    // ES001 is there, but does not fail the plan without --fail-on.
    assert!(
        results
            .iter()
            .any(|result| result["ruleId"] == "ES001" && result["level"] == "warning")
    );
    let sarif_path = dir.join("explainsql.sarif");
    let output = check(&["--format", "md", "--sarif", sarif_path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let markdown = stdout(&output);
    assert!(
        markdown.starts_with("<!-- explainsql check -->\n"),
        "{markdown}"
    );
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sarif_path).unwrap()).unwrap();
    assert_eq!(written, sarif);
    assert!(markdown.contains("**The plan failed.**"), "{markdown}");
    assert!(
        markdown.contains("| `plans/customer.txt` | **Failed** |"),
        "{markdown}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&check(&["--format", "json"]))).unwrap();
    assert_eq!(json["passed"], false);
    assert_eq!(json["plans"][0]["status"], "failed");

    // Findings fail a plan when asked to, even a new one.
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(check(&[]).status.code(), Some(0));
    let output = check(&["--fail-on", "high"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("ES001 Selective sequential scan"));

    // Errors are not failures.
    std::fs::write(&lock, "not a lock").unwrap();
    assert_eq!(check(&[]).status.code(), Some(2));
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(plans.join("notes.txt"), "not a plan").unwrap();
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("plans/notes.txt"));
    assert_eq!(run(&["check", "/nonexistent"], None).status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fails_on_findings_when_asked() {
    let scanned = run(&["--print", "--fail-on", "high"], Some(SCANNED));
    assert_eq!(scanned.status.code(), Some(1));
    assert!(stdout(&scanned).contains("ES001"));
    assert_eq!(
        run(&["--print", "--fail-on", "high"], Some(INDEXED))
            .status
            .code(),
        Some(0)
    );
    assert_eq!(run(&["--print"], Some(SCANNED)).status.code(), Some(0));
}

#[test]
fn checks_statements_against_a_database() {
    let Ok(url) = std::env::var("EXPLAINSQL_TEST_DATABASE_URL") else {
        eprintln!("EXPLAINSQL_TEST_DATABASE_URL is not set; skipping");
        return;
    };
    let dir = scratch("check-db");
    std::fs::write(
        dir.join("plans/customer.sql"),
        "SELECT id, amount FROM orders WHERE customer_id = 4242",
    )
    .unwrap();
    let lock = dir.join("explainsql.lock");
    let check = |extra: &[&str]| {
        let mut args = vec![
            "check",
            "-d",
            &url,
            dir.to_str().unwrap(),
            "--lock",
            lock.to_str().unwrap(),
            "--color",
            "never",
        ];
        args.extend_from_slice(extra);
        run(&args, None)
    };
    assert_eq!(check(&["--update"]).status.code(), Some(0));
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).starts_with("PASS  plans/customer.sql"));
    // The scan of orders is a finding: it fails when asked to, and the
    // suggested index is tested.
    let output = check(&["--fail-on", "high", "--prove"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("CREATE INDEX CONCURRENTLY ON public.orders (customer_id);"),
        "{text}"
    );
    assert!(text.contains("HypoPG"), "{text}");
    // A statement with parameters cannot run as it is: an error, which
    // says so, and the other statements are still checked.
    std::fs::write(
        dir.join("plans/by_status.sql"),
        "SELECT id FROM orders WHERE status = $1",
    )
    .unwrap();
    let output = check(&[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stdout(&output).contains("PASS  plans/customer.sql"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("takes parameters ($1)"),
        "{output:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The plans of server logs over time, from the logs captured in the three
/// formats PostgreSQL writes.
#[test]
fn tells_when_plans_in_logs_changed() {
    let log = fixture("logs/postgresql.log");
    let json = |args: &[&str]| -> serde_json::Value {
        let mut all = vec!["logs", log.to_str().unwrap(), "--format", "json"];
        all.extend_from_slice(args);
        let output = run(&all, None);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_str(&stdout(&output)).unwrap()
    };
    let report = json(&[]);
    let statements = report["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0]["prepared"], "latest");
    assert_eq!(statements[0]["changes"][0]["generic"], true);
    assert_eq!(statements[2]["pattern"], "stable");

    // Only the statements whose plan changed, or one of them by its tags.
    assert_eq!(
        json(&["--changed"])["statements"].as_array().unwrap().len(),
        2
    );
    let report = json(&["--query", "OrderController"]);
    assert_eq!(report["statements"].as_array().unwrap().len(), 1);
    assert_eq!(report["statements"][0]["runs"], 12);
    // Entries from a time on: the second run of the session.
    let report = json(&["--since", "2026-10-06 06:35:13.900"]);
    assert_eq!(report["entries"], 16);

    // The text report leads with the costliest change, and with --trace,
    // says which plan the trace ran.
    let output = run(
        &[
            "logs",
            log.to_str().unwrap(),
            "--trace",
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "--color",
            "never",
        ],
        None,
    );
    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.starts_with(
            "Trace 4bf92f3577b34da6a3ce929d0e0e4736: SELECT id, status, amount FROM orders"
        ),
        "{text}"
    );
    assert!(text.contains("ran plan 1 of 2"), "{text}");

    // A time it cannot read, and a file that is not a log.
    let output = run(
        &["logs", log.to_str().unwrap(), "--since", "yesterday"],
        None,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("give a time"));
    let plan = fixture("pg/16/seq_scan_selective.txt");
    let output = run(&["logs", plan.to_str().unwrap()], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no EXPLAIN plan found"));
}

/// A plan for sharing: names and values replaced, the report the same.
#[test]
fn anonymizes_a_plan() {
    let path = fixture("pg/16/seq_scan_selective.txt");
    let dir = std::env::temp_dir().join(format!("explainsql-anonymize-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let map = dir.join("map.json");
    let output = run(
        &[
            "anonymize",
            path.to_str().unwrap(),
            "--map",
            map.to_str().unwrap(),
        ],
        None,
    );
    assert!(output.status.success(), "{output:?}");
    let plan = stdout(&output);
    assert!(plan.starts_with("Seq Scan on public.table_a"), "{plan}");
    assert!(!plan.contains("orders"), "{plan}");
    let mapping: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&map).unwrap()).unwrap();
    assert_eq!(mapping["tables"]["orders"], "table_a");

    // The anonymized plan gets the same findings.
    let report = stdout(&run(&["--print"], Some(&plan)));
    assert!(
        report.contains("ES001 Selective sequential scan"),
        "{report}"
    );

    // Names kept on request; no plan, no output.
    let kept = stdout(&run(
        &["anonymize", "--keep-names", path.to_str().unwrap()],
        None,
    ));
    assert!(kept.starts_with("Seq Scan on public.orders"), "{kept}");
    let output = run(&["anonymize"], Some("hello"));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
