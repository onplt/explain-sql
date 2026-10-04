//! Removing what surrounds a plan in real-world input, and telling JSON
//! plans from text plans.
//!
//! Handled: Markdown code fences; auto_explain entries in jsonlog records and
//! in stderr logs; psql's aligned output (ASCII and Unicode line styles,
//! borders 0 to 2, `+`/`↵` continuation marks, wrapped lines); psql's
//! expanded output; prompts and other text before a text plan; CRLF line
//! endings, a byte order mark and non-breaking spaces.

use serde_json::Value;

use crate::ir::{Format, Warning, Wrapper};

pub(crate) struct Normalized {
    pub text: String,
    pub format: Format,
    pub wrappers: Vec<Wrapper>,
    /// The statement, when a log entry carried it.
    pub query_text: Option<String>,
    pub warnings: Vec<Warning>,
}

pub(crate) fn normalize(input: &str) -> Normalized {
    let mut text = clean(input);
    let mut wrappers = Vec::new();
    let mut warnings = Vec::new();
    let mut query_text = None;

    // Each pass removes one wrapper; wrappers nest (a fenced log excerpt).
    for _ in 0..8 {
        if let Some(inner) = strip_fence(&text) {
            wrappers.push(Wrapper::MarkdownFence);
            text = inner;
        } else if let Some((body, count)) = jsonlog_body(&text) {
            wrappers.push(Wrapper::JsonLog);
            note_extra_entries(count, &mut warnings);
            let (plan, query) = split_query_text(&body);
            query_text = query_text.or(query);
            text = plan;
        } else if let Some((body, count)) = log_body(&text) {
            wrappers.push(Wrapper::AutoExplainLog);
            note_extra_entries(count, &mut warnings);
            let (plan, query) = split_query_text(&body);
            query_text = query_text.or(query);
            text = plan;
        } else if let Some(inner) = psql_expanded(&text) {
            wrappers.push(Wrapper::PsqlExpanded);
            text = inner;
        } else if let Some((inner, wrapped)) = psql_table(&text) {
            wrappers.push(Wrapper::PsqlTable);
            if wrapped {
                wrappers.push(Wrapper::PsqlWrapped);
            }
            text = inner;
        } else {
            break;
        }
    }

    let format = if text.trim_start().starts_with(['[', '{']) {
        Format::Json
    } else {
        text = start_at_plan(&text, &mut warnings);
        Format::Text
    };
    Normalized {
        text,
        format,
        wrappers,
        query_text,
        warnings,
    }
}

/// Unifies line endings and removes a byte order mark, non-breaking spaces
/// and trailing spaces. Tabs are kept: a lone tab is an empty continuation
/// line in a server log.
fn clean(input: &str) -> String {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut text = String::with_capacity(input.len());
    for line in input.split('\n') {
        let line = line
            .strip_suffix('\r')
            .unwrap_or(line)
            .replace('\u{a0}', " ");
        // A lone CR also ends a line.
        for part in line.split('\r') {
            text.push_str(part.trim_end_matches(' '));
            text.push('\n');
        }
    }
    text.truncate(text.trim_end().len());
    text
}

fn note_extra_entries(count: usize, warnings: &mut Vec<Warning>) {
    if count > 1 {
        warnings.push(Warning {
            line: None,
            message: format!("the log contains {count} plans; showing the first"),
        });
    }
}

/// The content of the first Markdown code fence.
fn strip_fence(text: &str) -> Option<String> {
    let is_fence = |line: &&str| {
        let line = line.trim_start();
        line.starts_with("```") || line.starts_with("~~~")
    };
    let lines: Vec<&str> = text.lines().collect();
    let open = lines.iter().position(is_fence)?;
    let close = lines[open + 1..]
        .iter()
        .position(is_fence)
        .map_or(lines.len(), |offset| open + 1 + offset);
    Some(lines[open + 1..close].join("\n"))
}

/// The plan part of the first auto_explain message in jsonlog records, and
/// the number of such messages.
fn jsonlog_body(text: &str) -> Option<(String, usize)> {
    let mut bodies = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !line.starts_with('{') {
            return None;
        }
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        if let Some(body) = record
            .get("message")
            .and_then(Value::as_str)
            .and_then(auto_explain_body)
        {
            bodies.push(body.to_owned());
        }
    }
    let count = bodies.len();
    bodies.into_iter().next().map(|body| (body, count))
}

/// What follows `plan:` in an auto_explain message.
fn auto_explain_body(message: &str) -> Option<&str> {
    let start = message.find("plan:\n")?;
    message[..start]
        .contains("duration: ")
        .then(|| &message[start + "plan:\n".len()..])
}

/// The plan part of the first auto_explain entry in a stderr log, and the
/// number of entries.
fn log_body(text: &str) -> Option<(String, usize)> {
    let is_header = |line: &&str| line.ends_with("plan:") && line.contains("duration: ");
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(is_header)?;
    let count = lines.iter().filter(|line| is_header(line)).count();
    // Continuation lines of a log message start with a tab. If the tabs were
    // lost on the way, read until the next line that starts a log entry.
    let tabbed = lines
        .get(first + 1)
        .is_some_and(|line| line.starts_with('\t'));
    let mut body = Vec::new();
    for line in &lines[first + 1..] {
        if tabbed {
            match line.strip_prefix('\t') {
                Some(rest) => body.push(rest),
                None => break,
            }
        } else if starts_log_entry(line) {
            break;
        } else {
            body.push(line);
        }
    }
    Some((body.join("\n"), count))
}

/// Whether a line starts with a timestamp such as `2026-10-04 16:13:21`.
fn starts_log_entry(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() > 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
}

/// Separates auto_explain's `Query Text:` from a text plan. JSON plans carry
/// the query text inside the plan.
fn split_query_text(body: &str) -> (String, Option<String>) {
    if body.trim_start().starts_with(['{', '[']) || !body.starts_with("Query Text: ") {
        return (body.to_owned(), None);
    }
    // auto_explain always shows costs, so the plan starts at the first line
    // with a cost estimate; the query text may span several lines before it.
    let lines: Vec<&str> = body.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.contains("  (cost="))
        .unwrap_or(lines.len());
    let query = lines[..start].join("\n");
    let query = query.trim_start_matches("Query Text: ").trim().to_owned();
    (lines[start..].join("\n"), Some(query))
}

/// psql's expanded output (`\x`): `-[ RECORD n ]` lines and `QUERY PLAN | …`.
fn psql_expanded(text: &str) -> Option<String> {
    let is_record = |line: &&str| {
        let line = line.trim_start();
        line.starts_with("-[ RECORD ") || line.starts_with("─[ RECORD ")
    };
    let lines: Vec<&str> = text.lines().collect();
    if !lines.iter().any(is_record) {
        return None;
    }
    let mut out = Vec::new();
    for line in lines.iter().filter(|line| !is_record(line)) {
        let (label, value) = match line.split_once(" | ").or_else(|| line.split_once(" │ ")) {
            Some(parts) => parts,
            None => match line.strip_suffix(" |").or_else(|| line.strip_suffix(" │")) {
                Some(label) => (label, ""),
                None => continue,
            },
        };
        let label = label.trim();
        if label.is_empty() || label == "QUERY PLAN" {
            out.push(strip_continuation(value));
        }
    }
    Some(out.join("\n"))
}

/// psql's aligned output: an optional border, the `QUERY PLAN` header, a
/// separator, rows, and a `(n rows)` footer. Returns the rows and whether
/// wrapped lines were joined.
fn psql_table(text: &str) -> Option<(String, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    let header = lines
        .iter()
        .position(|line| strip_borders(line) == "QUERY PLAN")?;
    let bordered = lines[header].trim_start().starts_with(['|', '│']);
    let mut start = header + 1;
    let separated = lines.get(start).is_some_and(|line| is_separator(line));
    if separated {
        start += 1;
    }
    // With the default border, every row starts with a space.
    let padded =
        !bordered && separated && lines.get(start).is_some_and(|line| line.starts_with(' '));

    let mut out: Vec<String> = Vec::new();
    let mut wrapped = false;
    for line in &lines[start..] {
        if is_row_count(line) || (bordered && is_separator(line)) {
            break;
        }
        let row = if bordered {
            let row = line.trim_start();
            let row = row
                .strip_prefix("| ")
                .or_else(|| row.strip_prefix("│ "))
                .unwrap_or(row);
            row.strip_suffix('|')
                .or_else(|| row.strip_suffix('│'))
                .unwrap_or(row)
                .trim_end()
        } else if padded {
            if let Some(row) = line.strip_prefix(' ') {
                row
            } else if let Some(rest) = line.strip_prefix('.').or_else(|| line.strip_prefix('…')) {
                // A wrapped line continues; the previous row ends with the
                // same mark.
                if let Some(previous) = out.last_mut() {
                    if previous.ends_with(['.', '…']) {
                        previous.pop();
                    }
                    previous.push_str(rest);
                    wrapped = true;
                    continue;
                }
                rest
            } else {
                line
            }
        } else {
            line
        };
        out.push(strip_continuation(row).to_owned());
    }
    Some((out.join("\n"), wrapped))
}

fn strip_borders(line: &str) -> &str {
    line.trim()
        .trim_start_matches(['|', '│'])
        .trim_end_matches(['|', '│'])
        .trim()
}

fn is_separator(line: &str) -> bool {
    let line = line.trim();
    line.contains(['-', '─', '═']) && line.chars().all(|c| "-+─┼━╋═╪|│┌┐└┘├┤┬┴ ".contains(c))
}

fn is_row_count(line: &str) -> bool {
    let line = line.trim();
    line.strip_prefix('(')
        .and_then(|rest| {
            rest.strip_suffix(" rows)")
                .or_else(|| rest.strip_suffix(" row)"))
        })
        .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
}

/// Removes psql's mark for a value continuing on the next line.
fn strip_continuation(row: &str) -> &str {
    let row = row.trim_end();
    row.strip_suffix('+')
        .or_else(|| row.strip_suffix('↵'))
        .map_or(row, str::trim_end)
}

/// Drops lines before the first node of a text plan (a prompt, the EXPLAIN
/// command, log lines) and removes indentation shared by the whole plan.
fn start_at_plan(text: &str, warnings: &mut Vec<Warning>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .position(|line| {
            line.contains("  (cost=")
                || line.contains(" (actual ")
                || line.ends_with("(never executed)")
        })
        .or_else(|| lines.iter().position(|line| !line.trim().is_empty()))
        .unwrap_or(0);
    if lines[..first].iter().any(|line| !line.trim().is_empty()) {
        warnings.push(Warning {
            line: None,
            message: format!("ignored {first} line(s) before the plan"),
        });
    }
    let indent = lines
        .get(first)
        .map_or(0, |line| line.len() - line.trim_start_matches(' ').len());
    lines[first..]
        .iter()
        .map(|line| {
            let strip = indent.min(line.len() - line.trim_start_matches(' ').len());
            &line[strip..]
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_line_endings_and_odd_characters() {
        assert_eq!(clean("\u{feff}a  \r\nb\u{a0}c\rd\n\n"), "a\nb c\nd");
    }

    #[test]
    fn strips_a_markdown_fence() {
        let input = "Here is my plan:\n```sql\nSeq Scan on t  (cost=0.00..1.00 rows=1 width=4)\n```\nThanks";
        let normalized = normalize(input);
        assert_eq!(normalized.wrappers, [Wrapper::MarkdownFence]);
        assert_eq!(
            normalized.text,
            "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)"
        );
    }

    #[test]
    fn reads_psql_aligned_text_and_drops_the_prompt() {
        let input = "db=# EXPLAIN SELECT 1;\n     QUERY PLAN\n----------------\n Result  (cost=0.00..0.01 rows=1 width=4)\n   Output: 1\n(2 rows)\n\nTime: 0.3 ms";
        let normalized = normalize(input);
        assert_eq!(normalized.wrappers, [Wrapper::PsqlTable]);
        assert_eq!(normalized.format, Format::Text);
        assert_eq!(
            normalized.text,
            "Result  (cost=0.00..0.01 rows=1 width=4)\n  Output: 1"
        );
    }

    #[test]
    fn reads_psql_border_zero() {
        let input = "QUERY PLAN\n----------\nResult  (cost=0.00..0.01 rows=1 width=4)\n  Output: 1\n(2 rows)";
        assert_eq!(
            normalize(input).text,
            "Result  (cost=0.00..0.01 rows=1 width=4)\n  Output: 1"
        );
    }

    #[test]
    fn joins_psql_continuation_marks() {
        let input = "  QUERY PLAN\n------------\n [          +\n   {\"Plan\": +\n   1}       +\n ]\n(1 row)";
        let normalized = normalize(input);
        assert_eq!(normalized.format, Format::Json);
        assert_eq!(normalized.text, "[\n  {\"Plan\":\n  1}\n]");
    }

    #[test]
    fn detects_json_and_text() {
        assert_eq!(normalize("  [{\"Plan\": {}}]").format, Format::Json);
        assert_eq!(normalize("{\"Plan\": {}}").format, Format::Json);
        assert_eq!(
            normalize("Result  (cost=0.00..0.01 rows=1 width=4)").format,
            Format::Text
        );
    }

    #[test]
    fn removes_shared_indentation() {
        let normalized = normalize(
            "    Limit  (cost=0.00..1.00 rows=1 width=4)\n      ->  Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)",
        );
        assert_eq!(
            normalized.text,
            "Limit  (cost=0.00..1.00 rows=1 width=4)\n  ->  Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)"
        );
        assert!(normalized.warnings.is_empty());
    }

    #[test]
    fn splits_a_multiline_query_text() {
        let body = "Query Text: SELECT *\nFROM t\nResult  (cost=0.00..0.01 rows=1 width=4)";
        let (plan, query) = split_query_text(body);
        assert_eq!(plan, "Result  (cost=0.00..0.01 rows=1 width=4)");
        assert_eq!(query.as_deref(), Some("SELECT *\nFROM t"));
    }

    #[test]
    fn recognizes_row_counts_and_separators() {
        assert!(is_row_count("(11 rows)"));
        assert!(is_row_count("(1 row)"));
        assert!(!is_row_count("(rows)"));
        assert!(is_separator("+------+"));
        assert!(is_separator("───────"));
        assert!(!is_separator("  Filter: (a - b)"));
    }
}
