//! Scenario files: one SQL statement per file in `fixtures/scenarios/`,
//! preceded by a header of `-- key: value` directives.
//!
//! ```sql
//! -- description: Sequential scan whose filter keeps 10 of 200,000 rows.
//! -- rules: ES001
//! -- advice: index
//! -- set: max_parallel_workers_per_gather = 0
//! SELECT * FROM orders WHERE customer_id = 4242;
//! ```
//!
//! The directives are documented in `fixtures/README.md`.

use std::fs;
use std::path::Path;

/// EXPLAIN options used when a scenario has no `options` directive.
pub const DEFAULT_OPTIONS: &str = "ANALYZE, BUFFERS, VERBOSE, SETTINGS";

/// The oldest PostgreSQL major version in the corpus, and the default `min_version`.
pub const OLDEST_VERSION: u32 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    /// IDs of the rules in `docs/rules.md` that this plan triggers; no other
    /// rule may fire. An ID ending in `?` may or may not fire, depending on
    /// the server version's estimates.
    pub rules: Vec<String>,
    /// What the index advisor is expected to conclude, when the scenario pins it down.
    pub advice: Option<Advice>,
    pub min_version: u32,
    pub requires_jit: bool,
    /// `name = value` pairs, applied with `SET` before the statement.
    pub settings: Vec<String>,
    /// EXPLAIN options, without `FORMAT` (the generator adds it).
    pub options: String,
    pub statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    /// An index candidate is expected (`advice: index`).
    IndexCandidate,
    /// No index should be suggested; a trap for naive advisors (`advice: none`).
    NoSuggestion,
    /// The fix is a query rewrite rather than an index (`advice: rewrite`).
    QueryRewrite,
}

impl Advice {
    pub fn as_str(self) -> &'static str {
        match self {
            Advice::IndexCandidate => "index",
            Advice::NoSuggestion => "none",
            Advice::QueryRewrite => "rewrite",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "index" => Some(Advice::IndexCandidate),
            "none" => Some(Advice::NoSuggestion),
            "rewrite" => Some(Advice::QueryRewrite),
            _ => None,
        }
    }
}

impl Scenario {
    pub fn parse(name: &str, source: &str) -> Result<Self, String> {
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid_name {
            return Err(format!(
                "scenario name `{name}` must only use a-z, 0-9 and _"
            ));
        }

        let mut description = None;
        let mut rules = Vec::new();
        let mut advice = None;
        let mut min_version = OLDEST_VERSION;
        let mut requires_jit = false;
        let mut settings = Vec::new();
        let mut options = None;
        let mut seen: Vec<&str> = Vec::new();

        let lines: Vec<&str> = source.lines().collect();
        let header_len = lines
            .iter()
            .take_while(|line| line.starts_with("--"))
            .count();

        for (index, line) in lines[..header_len].iter().enumerate() {
            let number = index + 1;
            let directive = line.trim_start_matches('-').trim();
            let (key, value) = directive
                .split_once(':')
                .ok_or_else(|| format!("line {number}: expected `-- key: value`"))?;
            let (key, value) = (key.trim(), value.trim());
            if value.is_empty() {
                return Err(format!("line {number}: `{key}` has no value"));
            }
            if key != "set" {
                if seen.contains(&key) {
                    return Err(format!("line {number}: `{key}` is given more than once"));
                }
                seen.push(key);
            }
            match key {
                "description" => description = Some(value.to_owned()),
                "rules" => {
                    for rule in value.split(',').map(str::trim) {
                        let id = rule.strip_suffix('?').unwrap_or(rule);
                        let valid = id.len() == 5
                            && id.starts_with("ES")
                            && id[2..].bytes().all(|b| b.is_ascii_digit());
                        if !valid {
                            return Err(format!(
                                "line {number}: `{rule}` is not a rule ID like ES001 or ES001?"
                            ));
                        }
                        rules.push(rule.to_owned());
                    }
                }
                "advice" => {
                    advice = Some(Advice::parse(value).ok_or_else(|| {
                        format!("line {number}: advice must be index, none or rewrite")
                    })?);
                }
                "min_version" => {
                    min_version = value.parse().map_err(|_| {
                        format!("line {number}: min_version must be a major version number")
                    })?;
                }
                "requires" => match value {
                    "jit" => requires_jit = true,
                    other => {
                        return Err(format!(
                            "line {number}: unknown requirement `{other}` (supported: jit)"
                        ));
                    }
                },
                "set" => settings.push(value.to_owned()),
                "options" => {
                    if value.to_ascii_uppercase().contains("FORMAT") {
                        return Err(format!(
                            "line {number}: leave FORMAT out of options; the generator adds it"
                        ));
                    }
                    options = Some(value.to_owned());
                }
                other => return Err(format!("line {number}: unknown directive `{other}`")),
            }
        }

        let statement = lines[header_len..].join("\n");
        let statement = statement.trim().trim_end_matches(';').trim_end();
        if statement.is_empty() {
            return Err("no SQL statement after the header".to_owned());
        }

        Ok(Scenario {
            name: name.to_owned(),
            description: description.ok_or("missing `description` directive")?,
            rules,
            advice,
            min_version,
            requires_jit,
            settings,
            options: options.unwrap_or_else(|| DEFAULT_OPTIONS.to_owned()),
            statement: statement.to_owned(),
        })
    }

    /// Whether `EXPLAIN` runs the statement (the `ANALYZE` option).
    pub fn executes(&self) -> bool {
        self.options.split(',').any(|option| {
            let mut words = option.split_whitespace();
            words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("ANALYZE"))
                && !words.next().is_some_and(|value| {
                    matches!(value.to_ascii_lowercase().as_str(), "false" | "off" | "0")
                })
        })
    }

    /// Whether the statement modifies data. Such statements are rolled back,
    /// but they still clear visibility map bits on the pages they touch, so
    /// the generator runs them after the read-only scenarios.
    pub fn modifies_data(&self) -> bool {
        let keyword = self.statement.split_whitespace().next().unwrap_or_default();
        ["INSERT", "UPDATE", "DELETE", "MERGE"]
            .iter()
            .any(|dml| keyword.eq_ignore_ascii_case(dml))
    }

    /// The psql script that captures this scenario in the given EXPLAIN format.
    /// It runs inside a transaction that is always rolled back.
    pub fn script(&self, format: &str) -> String {
        let mut script = String::from("BEGIN;\n");
        for setting in &self.settings {
            script.push_str("SET ");
            script.push_str(setting);
            script.push_str(";\n");
        }
        script.push_str(&format!(
            "EXPLAIN ({}, FORMAT {format})\n{};\nROLLBACK;\n",
            self.options, self.statement
        ));
        script
    }

    /// Why this scenario cannot be captured on the given server, if it cannot.
    pub fn skip_reason(&self, major: u32, jit_available: bool) -> Option<String> {
        if major < self.min_version {
            Some(format!("requires PostgreSQL {} or later", self.min_version))
        } else if self.requires_jit && !jit_available {
            Some("requires a server built with JIT support".to_owned())
        } else {
            None
        }
    }
}

/// Loads every `*.sql` file in `dir`, sorted by name.
pub fn load_all(dir: &Path) -> Result<Vec<Scenario>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut scenarios = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().is_none_or(|ext| ext != "sql") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("{}: file name is not UTF-8", path.display()))?;
        let source = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let scenario =
            Scenario::parse(name, &source).map_err(|e| format!("{}: {e}", path.display()))?;
        scenarios.push(scenario);
    }
    if scenarios.is_empty() {
        return Err(format!("no scenario files in {}", dir.display()));
    }
    scenarios.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(scenarios)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_directive() {
        let source = "\
-- description: Hash join that spills to disk.
-- rules: ES004, ES002?
-- advice: index
-- min_version: 13
-- requires: jit
-- set: work_mem = '64kB'
-- set: enable_mergejoin = off
-- options: ANALYZE, BUFFERS
SELECT count(*)
FROM orders o
JOIN order_items oi ON oi.order_id = o.id;
";
        let scenario = Scenario::parse("hash_join_batches", source).unwrap();
        assert_eq!(
            scenario,
            Scenario {
                name: "hash_join_batches".to_owned(),
                description: "Hash join that spills to disk.".to_owned(),
                rules: vec!["ES004".to_owned(), "ES002?".to_owned()],
                advice: Some(Advice::IndexCandidate),
                min_version: 13,
                requires_jit: true,
                settings: vec![
                    "work_mem = '64kB'".to_owned(),
                    "enable_mergejoin = off".to_owned()
                ],
                options: "ANALYZE, BUFFERS".to_owned(),
                statement:
                    "SELECT count(*)\nFROM orders o\nJOIN order_items oi ON oi.order_id = o.id"
                        .to_owned(),
            }
        );
    }

    #[test]
    fn applies_defaults() {
        let scenario = Scenario::parse("pk", "-- description: Lookup.\nSELECT 1\r\n").unwrap();
        assert_eq!(scenario.options, DEFAULT_OPTIONS);
        assert_eq!(scenario.min_version, OLDEST_VERSION);
        assert!(scenario.rules.is_empty());
        assert_eq!(scenario.advice, None);
        assert!(!scenario.requires_jit);
        assert_eq!(scenario.statement, "SELECT 1");
    }

    #[test]
    fn rejects_invalid_headers() {
        let cases = [
            ("SELECT 1", "missing `description`"),
            (
                "-- description: x\n-- colour: red\nSELECT 1",
                "unknown directive",
            ),
            (
                "-- description: x\n-- description: y\nSELECT 1",
                "more than once",
            ),
            ("-- description: x\n-- rules: E1\nSELECT 1", "not a rule ID"),
            (
                "-- description: x\n-- advice: maybe\nSELECT 1",
                "advice must be",
            ),
            (
                "-- description: x\n-- min_version: new\nSELECT 1",
                "major version",
            ),
            (
                "-- description: x\n-- requires: gpu\nSELECT 1",
                "unknown requirement",
            ),
            (
                "-- description: x\n-- options: FORMAT JSON\nSELECT 1",
                "leave FORMAT out",
            ),
            ("-- description\nSELECT 1", "expected `-- key: value`"),
            ("-- description: x\n;\n", "no SQL statement"),
        ];
        for (source, expected) in cases {
            let error = Scenario::parse("case", source).unwrap_err();
            assert!(error.contains(expected), "{source:?}: {error}");
        }
        assert!(Scenario::parse("Bad-Name", "-- description: x\nSELECT 1").is_err());
    }

    #[test]
    fn detects_whether_explain_executes_the_statement() {
        let with_options = |options: &str| {
            Scenario::parse(
                "s",
                &format!("-- description: x\n-- options: {options}\nSELECT 1"),
            )
            .unwrap()
            .executes()
        };
        assert!(with_options("ANALYZE, BUFFERS"));
        assert!(with_options("buffers, analyze true"));
        assert!(!with_options("VERBOSE, SETTINGS"));
        assert!(!with_options("ANALYZE false, VERBOSE"));
        assert!(!with_options("GENERIC_PLAN"));
    }

    #[test]
    fn detects_data_modifying_statements() {
        let statement = |sql: &str| {
            Scenario::parse("s", &format!("-- description: x\n{sql}"))
                .unwrap()
                .modifies_data()
        };
        assert!(statement("DELETE FROM orders WHERE id > 1"));
        assert!(statement("insert into t select 1"));
        assert!(statement("UPDATE orders\nSET note = ''"));
        assert!(!statement("SELECT * FROM orders"));
        assert!(!statement("WITH x AS (SELECT 1) SELECT * FROM x"));
    }

    #[test]
    fn builds_a_rolled_back_script() {
        let scenario = Scenario::parse(
            "s",
            "-- description: x\n-- set: work_mem = '64kB'\nSELECT 1;\n",
        )
        .unwrap();
        assert_eq!(
            scenario.script("JSON"),
            "BEGIN;\nSET work_mem = '64kB';\n\
             EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON)\nSELECT 1;\nROLLBACK;\n"
        );
    }

    #[test]
    fn explains_why_a_scenario_is_skipped() {
        let scenario = Scenario::parse(
            "s",
            "-- description: x\n-- min_version: 14\n-- requires: jit\nSELECT 1",
        )
        .unwrap();
        assert_eq!(
            scenario.skip_reason(13, true).as_deref(),
            Some("requires PostgreSQL 14 or later")
        );
        assert_eq!(
            scenario.skip_reason(14, false).as_deref(),
            Some("requires a server built with JIT support")
        );
        assert_eq!(scenario.skip_reason(14, true), None);
    }
}
