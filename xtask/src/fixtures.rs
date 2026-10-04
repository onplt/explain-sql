//! The EXPLAIN fixture corpus in `fixtures/`.
//!
//! `cargo xtask gen-fixtures` starts a throwaway Docker container for every
//! PostgreSQL major version, loads `fixtures/schema.sql`, and captures every
//! scenario in `fixtures/scenarios/` as JSON and text EXPLAIN output under
//! `fixtures/pg/<major>/`, together with a `manifest.json` describing the
//! server and the status of each scenario.
//!
//! `cargo xtask check-fixtures` (and the test at the bottom of this file)
//! checks that the committed corpus still matches the scenario files.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::scenario::{self, Advice, Scenario};

/// PostgreSQL major versions in the corpus.
pub const DEFAULT_VERSIONS: [u32; 7] = [12, 13, 14, 15, 16, 17, 18];

/// Server settings for the fixture containers. Autovacuum is off so that the
/// statistics and the visibility map stay exactly as `schema.sql` leaves them.
/// A small `shared_buffers` makes sequential scans of the larger tables read
/// through a ring buffer, as they do on production-sized tables.
const SERVER_SETTINGS: &[&str] = &[
    "autovacuum=off",
    "track_io_timing=on",
    "shared_buffers=32MB",
    "fsync=off",
    "synchronous_commit=off",
];

const READY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Options {
    pub versions: Vec<u32>,
    pub only: Option<Vec<String>>,
    pub keep_containers: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Options {
            versions: DEFAULT_VERSIONS.to_vec(),
            only: None,
            keep_containers: false,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--versions" => {
                    let value = args.next().ok_or("--versions needs a value")?;
                    options.versions = value
                        .split(',')
                        .map(|v| {
                            v.trim()
                                .parse()
                                .map_err(|_| format!("invalid PostgreSQL major version `{v}`"))
                        })
                        .collect::<Result<_, _>>()?;
                }
                "--only" => {
                    let value = args.next().ok_or("--only needs a value")?;
                    options.only = Some(value.split(',').map(|s| s.trim().to_owned()).collect());
                }
                "--keep-containers" => options.keep_containers = true,
                other => return Err(format!("unknown option `{other}` for gen-fixtures")),
            }
        }
        Ok(options)
    }
}

pub fn generate(options: &Options) -> Result<(), String> {
    let root = workspace_root();
    let scenarios = scenario::load_all(&root.join("fixtures/scenarios"))?;
    let selected: Vec<&Scenario> = match &options.only {
        None => scenarios.iter().collect(),
        Some(names) => {
            if let Some(unknown) = names
                .iter()
                .find(|n| !scenarios.iter().any(|s| &s.name == *n))
            {
                return Err(format!("unknown scenario `{unknown}`"));
            }
            scenarios
                .iter()
                .filter(|s| names.contains(&s.name))
                .collect()
        }
    };
    let schema_path = root.join("fixtures/schema.sql");
    let schema =
        fs::read_to_string(&schema_path).map_err(|e| format!("{}: {e}", schema_path.display()))?;
    docker(&["version", "--format", "{{.Server.Version}}"])
        .map_err(|e| format!("a running Docker daemon is required: {e}"))?;

    let mut failures = Vec::new();
    for &major in &options.versions {
        failures.extend(generate_version(
            &root, major, &scenarios, &selected, &schema, options,
        )?);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} scenario(s) failed:\n  {}",
            failures.len(),
            failures.join("\n  ")
        ))
    }
}

/// Captures the selected scenarios on one PostgreSQL major version and
/// returns the failures.
fn generate_version(
    root: &Path,
    major: u32,
    scenarios: &[Scenario],
    selected: &[&Scenario],
    schema: &str,
    options: &Options,
) -> Result<Vec<String>, String> {
    let image = format!("postgres:{major}");
    ensure_image(&image)?;
    println!("PostgreSQL {major}: starting {image}");
    let container = Container::start(major, &image, options.keep_containers)?;
    container.wait_until_ready()?;
    container
        .psql(schema)
        .map_err(|e| format!("PostgreSQL {major}: loading schema.sql failed: {e}"))?;

    let server_version = container.query("SHOW server_version")?;
    let server_version_num: u64 = container
        .query("SHOW server_version_num")?
        .parse()
        .map_err(|e| format!("unexpected server_version_num: {e}"))?;
    let jit_available = container.query("SELECT pg_jit_available()")? == "t";
    let image_digest = docker(&[
        "image",
        "inspect",
        "--format",
        "{{index .RepoDigests 0}}",
        image.as_str(),
    ])
    .unwrap_or_default();
    println!("PostgreSQL {major}: server {server_version}, JIT available: {jit_available}");

    let dir = root.join("fixtures/pg").join(major.to_string());
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let manifest_path = dir.join("manifest.json");

    // A partial run (--only) keeps the entries of the other scenarios.
    let mut entries: BTreeMap<String, Value> = match &options.only {
        Some(_) => read_manifest(&manifest_path)
            .ok()
            .and_then(|manifest| manifest["scenarios"].as_object().cloned())
            .map(|object| object.into_iter().collect())
            .unwrap_or_default(),
        None => {
            remove_stale_files(&dir, scenarios)?;
            BTreeMap::new()
        }
    };
    entries.retain(|name, _| scenarios.iter().any(|s| &s.name == name));

    // Read-only scenarios first: see Scenario::modifies_data.
    let ordered = selected
        .iter()
        .filter(|s| !s.modifies_data())
        .chain(selected.iter().filter(|s| s.modifies_data()));

    let mut failures = Vec::new();
    for scenario in ordered {
        let started = Instant::now();
        let entry = if let Some(reason) = scenario.skip_reason(major, jit_available) {
            remove_plans(&dir, &scenario.name)?;
            println!("  {:<40} skipped: {reason}", scenario.name);
            manifest_entry(scenario, "skipped", Some(&reason))
        } else {
            match capture(&container, scenario) {
                Ok((json, text)) => {
                    write_file(&dir.join(format!("{}.json", scenario.name)), &json)?;
                    write_file(&dir.join(format!("{}.txt", scenario.name)), &text)?;
                    let seconds = started.elapsed().as_secs_f64();
                    println!("  {:<40} ok ({seconds:.1} s)", scenario.name);
                    manifest_entry(scenario, "generated", None)
                }
                Err(error) => {
                    remove_plans(&dir, &scenario.name)?;
                    println!("  {:<40} FAILED", scenario.name);
                    failures.push(format!("PostgreSQL {major} / {}: {error}", scenario.name));
                    manifest_entry(scenario, "failed", Some(&error))
                }
            }
        };
        entries.insert(scenario.name.clone(), entry);
    }

    let manifest = json!({
        "postgres": {
            "major": major,
            "server_version": server_version,
            "server_version_num": server_version_num,
            "jit_available": jit_available,
            "image": image,
            "image_digest": image_digest,
        },
        "scenarios": entries,
    });
    let manifest = serde_json::to_string_pretty(&manifest).expect("manifest serializes");
    write_file(&manifest_path, &manifest)?;
    Ok(failures)
}

/// Runs a scenario and returns its JSON and text plans.
fn capture(container: &Container, scenario: &Scenario) -> Result<(String, String), String> {
    if scenario.executes() {
        // A discarded warm-up run, so that both captured runs see the same cache state.
        container.psql(&scenario.script("TEXT"))?;
    }
    let json = container.psql(&scenario.script("JSON"))?;
    check_json_plan(&json, scenario.executes())?;
    let text = container.psql(&scenario.script("TEXT"))?;
    if text.trim().is_empty() {
        return Err("psql returned an empty text plan".to_owned());
    }
    Ok((json, text))
}

fn manifest_entry(scenario: &Scenario, status: &str, reason: Option<&str>) -> Value {
    let mut entry = json!({
        "status": status,
        "description": scenario.description,
        "rules": scenario.rules,
        "advice": scenario.advice.map(Advice::as_str),
    });
    if let Some(reason) = reason {
        entry["reason"] = json!(reason);
    }
    entry
}

/// Checks that `json` is what `EXPLAIN (FORMAT JSON)` prints: an array with
/// exactly one object holding a `Plan`, and an `Execution Time` when the
/// statement was executed.
pub fn check_json_plan(json: &str, executed: bool) -> Result<(), String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("invalid JSON plan: {e}"))?;
    let [entry] = value.as_array().map(Vec::as_slice).unwrap_or_default() else {
        return Err("expected a JSON array holding exactly one plan".to_owned());
    };
    if !entry.get("Plan").is_some_and(Value::is_object) {
        return Err("the JSON plan has no \"Plan\" object".to_owned());
    }
    if executed && entry.get("Execution Time").is_none() {
        return Err("the JSON plan has no \"Execution Time\" although ANALYZE ran".to_owned());
    }
    Ok(())
}

/// `cargo xtask check-fixtures`.
pub fn check() -> Result<(), String> {
    let summary = check_corpus(&workspace_root())
        .map_err(|problems| format!("fixture corpus problems:\n  {}", problems.join("\n  ")))?;
    println!("{summary}");
    Ok(())
}

/// Checks the committed corpus against the scenario files and returns a
/// summary, or every problem found.
pub fn check_corpus(root: &Path) -> Result<String, Vec<String>> {
    let scenarios = scenario::load_all(&root.join("fixtures/scenarios")).map_err(|e| vec![e])?;
    let pg_dir = root.join("fixtures/pg");
    let mut problems = Vec::new();
    let mut plan_pairs = 0;

    if let Ok(entries) = fs::read_dir(&pg_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let expected = name
                .parse::<u32>()
                .is_ok_and(|major| DEFAULT_VERSIONS.contains(&major));
            if !expected {
                problems.push(format!("{}: unexpected entry", entry.path().display()));
            }
        }
    }

    for major in DEFAULT_VERSIONS {
        let dir = pg_dir.join(major.to_string());
        let manifest_path = dir.join("manifest.json");
        let manifest = match read_manifest(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                problems.push(format!("{error} (regenerate the corpus)"));
                continue;
            }
        };
        let jit_available = manifest["postgres"]["jit_available"].as_bool() == Some(true);
        let empty = serde_json::Map::new();
        let entries = manifest["scenarios"].as_object().unwrap_or(&empty);

        for scenario in &scenarios {
            let label = format!("PostgreSQL {major} / {}", scenario.name);
            let Some(entry) = entries.get(&scenario.name) else {
                problems.push(format!(
                    "{label}: missing from manifest.json (regenerate it)"
                ));
                continue;
            };
            let expected = manifest_entry(scenario, "", None);
            if ["description", "rules", "advice"]
                .iter()
                .any(|key| entry[key] != expected[key])
            {
                problems.push(format!(
                    "{label}: manifest.json is out of date with the scenario file (regenerate it)"
                ));
            }
            let json_path = dir.join(format!("{}.json", scenario.name));
            let text_path = dir.join(format!("{}.txt", scenario.name));
            match scenario.skip_reason(major, jit_available) {
                Some(reason) => {
                    if entry["status"] != "skipped" || entry["reason"] != reason {
                        problems.push(format!("{label}: expected to be skipped: {reason}"));
                    }
                    if json_path.exists() || text_path.exists() {
                        problems.push(format!("{label}: skipped, but plan files exist"));
                    }
                }
                None => {
                    if entry["status"] != "generated" {
                        problems.push(format!(
                            "{label}: status is {}, expected \"generated\"",
                            entry["status"]
                        ));
                        continue;
                    }
                    match fs::read_to_string(&json_path) {
                        Ok(json) => {
                            if let Err(error) = check_json_plan(&json, scenario.executes()) {
                                problems.push(format!("{label}: {error}"));
                            }
                        }
                        Err(error) => problems.push(format!("{}: {error}", json_path.display())),
                    }
                    match fs::read_to_string(&text_path) {
                        Ok(text) if text.trim().is_empty() => {
                            problems.push(format!("{label}: the text plan is empty"));
                        }
                        Ok(_) => {}
                        Err(error) => problems.push(format!("{}: {error}", text_path.display())),
                    }
                    plan_pairs += 1;
                }
            }
        }

        for name in entries.keys() {
            if !scenarios.iter().any(|s| &s.name == name) {
                problems.push(format!(
                    "PostgreSQL {major}: manifest.json lists unknown scenario `{name}`"
                ));
            }
        }
        if let Ok(files) = fs::read_dir(&dir) {
            for file in files.flatten() {
                let file_name = file.file_name().to_string_lossy().into_owned();
                let expected = file_name == "manifest.json"
                    || file_name.rsplit_once('.').is_some_and(|(stem, ext)| {
                        matches!(ext, "json" | "txt")
                            && scenarios.iter().any(|s| {
                                s.name == stem && s.skip_reason(major, jit_available).is_none()
                            })
                    });
                if !expected {
                    problems.push(format!("{}: unexpected file", file.path().display()));
                }
            }
        }
    }

    if problems.is_empty() {
        let versions: Vec<String> = DEFAULT_VERSIONS.iter().map(u32::to_string).collect();
        Ok(format!(
            "{} scenarios, {plan_pairs} JSON/text plan pairs across PostgreSQL {}",
            scenarios.len(),
            versions.join(", ")
        ))
    } else {
        Err(problems)
    }
}

fn read_manifest(path: &Path) -> Result<Value, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&source).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `contents` with exactly one trailing newline.
fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    let mut contents = contents.trim_end().to_owned();
    contents.push('\n');
    fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))
}

fn remove_plans(dir: &Path, name: &str) -> Result<(), String> {
    for extension in ["json", "txt"] {
        let path = dir.join(format!("{name}.{extension}"));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Ok(())
}

/// Removes plan files of scenarios that no longer exist.
fn remove_stale_files(dir: &Path, scenarios: &[Scenario]) -> Result<(), String> {
    let files = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for file in files.flatten() {
        let file_name = file.file_name().to_string_lossy().into_owned();
        let current = file_name == "manifest.json"
            || file_name.rsplit_once('.').is_some_and(|(stem, ext)| {
                matches!(ext, "json" | "txt") && scenarios.iter().any(|s| s.name == stem)
            });
        if !current {
            let path = file.path();
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("  removed stale {}", path.display());
        }
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

/// Runs `docker` and returns its trimmed standard output.
fn docker(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run docker: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "`docker {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn ensure_image(image: &str) -> Result<(), String> {
    if docker(&["image", "inspect", image]).is_ok() {
        return Ok(());
    }
    println!("Pulling {image}");
    let status = Command::new("docker")
        .args(["pull", image])
        .status()
        .map_err(|e| format!("failed to run docker: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`docker pull {image}` failed"))
    }
}

/// A PostgreSQL container that is removed when dropped.
struct Container {
    name: String,
    keep: bool,
}

impl Container {
    fn start(major: u32, image: &str, keep: bool) -> Result<Self, String> {
        let name = format!("explainsql-fixtures-pg{major}-{}", std::process::id());
        let mut args = vec![
            "run",
            "--detach",
            "--name",
            name.as_str(),
            "--shm-size=512m",
            "--env",
            "POSTGRES_HOST_AUTH_METHOD=trust",
            image,
        ];
        for &setting in SERVER_SETTINGS {
            args.extend(["-c", setting]);
        }
        docker(&args)?;
        Ok(Container { name, keep })
    }

    fn wait_until_ready(&self) -> Result<(), String> {
        // The image's entrypoint runs initdb with a temporary server that only
        // listens on the Unix socket, so a TCP check succeeds only once the
        // real server is up.
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let ready = Command::new("docker")
                .args([
                    "exec",
                    self.name.as_str(),
                    "pg_isready",
                    "--quiet",
                    "--host",
                    "127.0.0.1",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if ready {
                return Ok(());
            }
            let running = docker(&[
                "inspect",
                "--format",
                "{{.State.Running}}",
                self.name.as_str(),
            ])
            .is_ok_and(|state| state == "true");
            if !running {
                return Err(format!("container {} exited:\n{}", self.name, self.logs()));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "PostgreSQL in {} was not ready after {} s:\n{}",
                    self.name,
                    READY_TIMEOUT.as_secs(),
                    self.logs()
                ));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn logs(&self) -> String {
        Command::new("docker")
            .args(["logs", "--tail", "40", self.name.as_str()])
            .output()
            .map(|output| {
                let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
                logs.push_str(&String::from_utf8_lossy(&output.stderr));
                logs
            })
            .unwrap_or_default()
    }

    /// Runs a psql script with unaligned, tuples-only output and stops at the
    /// first error.
    fn psql(&self, script: &str) -> Result<String, String> {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "--interactive",
                self.name.as_str(),
                "psql",
                "--host",
                "127.0.0.1",
                "--username",
                "postgres",
                "--dbname",
                "postgres",
                "--no-psqlrc",
                "--quiet",
                "--no-align",
                "--tuples-only",
                "--set",
                "ON_ERROR_STOP=1",
                "--file",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run docker exec: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let script = script.to_owned();
        let writer = thread::spawn(move || stdin.write_all(script.as_bytes()));
        let output = child
            .wait_with_output()
            .map_err(|e| format!("failed to wait for psql: {e}"))?;
        let written = writer.join().expect("the psql stdin writer does not panic");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(stderr.trim().to_owned());
        }
        written.map_err(|e| format!("failed to write to psql: {e}"))?;
        for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
            eprintln!("    psql: {line}");
        }
        String::from_utf8(output.stdout).map_err(|e| format!("psql output is not UTF-8: {e}"))
    }

    /// Runs one statement and returns its single value.
    fn query(&self, sql: &str) -> Result<String, String> {
        Ok(self.psql(&format!("{sql};\n"))?.trim().to_owned())
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.keep {
            println!(
                "  kept container {0}; remove it with: docker rm -f {0}",
                self.name
            );
        } else {
            let _ = Command::new("docker")
                .args(["rm", "--force", self.name.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_corpus_matches_the_scenarios() {
        if let Err(problems) = check_corpus(&workspace_root()) {
            panic!("fixture corpus problems:\n  {}", problems.join("\n  "));
        }
    }

    #[test]
    fn accepts_only_single_plan_json() {
        let plan = r#"[{"Plan": {"Node Type": "Result"}, "Execution Time": 0.01}]"#;
        assert!(check_json_plan(plan, true).is_ok());
        assert!(check_json_plan(r#"[{"Plan": {}}]"#, false).is_ok());
        assert!(check_json_plan(r#"[{"Plan": {}}]"#, true).is_err());
        assert!(check_json_plan(r#"[]"#, false).is_err());
        assert!(check_json_plan(r#"{"Plan": {}}"#, false).is_err());
        assert!(check_json_plan("Seq Scan on t", false).is_err());
    }

    #[test]
    fn parses_gen_fixtures_options() {
        let args: Vec<String> = ["--versions", "16, 17", "--only", "a,b", "--keep-containers"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let options = Options::parse(&args).unwrap();
        assert_eq!(options.versions, [16, 17]);
        assert_eq!(options.only, Some(vec!["a".to_owned(), "b".to_owned()]));
        assert!(options.keep_containers);
        assert!(Options::parse(&["--versions".to_owned(), "x".to_owned()]).is_err());
        assert!(Options::parse(&["--bogus".to_owned()]).is_err());
    }
}
