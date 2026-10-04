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
