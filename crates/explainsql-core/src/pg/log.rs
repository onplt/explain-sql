//! Server logs, entry by entry: each auto_explain plan with what the log
//! says about it, from the fields of jsonlog and csvlog records, or from
//! the line prefix of a stderr log (`log_line_prefix`).

use serde::Serialize;
use serde_json::Value;

use super::normalize;
use crate::ir::{Plan, Wrapper};

/// An auto_explain entry of a server log: the plan, and what the log says
/// about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogEntry {
    pub meta: LogMeta,
    /// The values a prepared statement ran with, as auto_explain logs them
    /// from PostgreSQL 16: `$1 = '777', $2 = '10'`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    pub plan: Plan,
}

/// What a server log says about an auto_explain entry, besides the plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LogMeta {
    /// As the log prints it: `2026-10-06 06:35:13.659 UTC`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// The statement's duration as auto_explain logged it, in thousandths
    /// of a millisecond, so that entries compare exactly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_us: Option<u64>,
    /// The log's query identifier (jsonlog and csvlog, PostgreSQL 14+),
    /// when it is not zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_id: Option<i64>,
    /// The 1-based line of the log the entry starts on.
    pub line: usize,
    /// The log's file, when the reader says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

impl LogMeta {
    /// The duration in milliseconds.
    pub fn duration(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        self.duration_us.map(|us| us as f64 / 1000.0)
    }
}

/// An entry as the log holds it: what the log says, and the plan's text.
pub(super) struct Record {
    pub meta: LogMeta,
    pub body: String,
    pub wrapper: Wrapper,
}

/// Every auto_explain entry of a log, in the log's order; `None` when the
/// text is not a server log with auto_explain entries.
pub(super) fn records(text: &str) -> Option<Vec<Record>> {
    jsonlog(text)
        .or_else(|| csvlog(text))
        .or_else(|| stderr(text))
}

fn jsonlog(text: &str) -> Option<Vec<Record>> {
    let mut records = Vec::new();
    let mut first = true;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The first record says whether this is a jsonlog; after it, a
        // record cut short, as the last one of a log being written, is left
        // out.
        let record = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(record)) => record,
            _ if first => return None,
            _ => continue,
        };
        first = false;
        let Some(message) = record.get("message").and_then(Value::as_str) else {
            continue;
        };
        let Some(body) = normalize::auto_explain_body(message) else {
            continue;
        };
        let field = |key: &str| {
            record
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        records.push(Record {
            meta: LogMeta {
                timestamp: field("timestamp"),
                pid: record
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok()),
                user: field("user"),
                database: field("dbname"),
                application: field("application_name"),
                duration_us: duration(message),
                query_id: record
                    .get("query_id")
                    .and_then(Value::as_i64)
                    .filter(|&id| id != 0),
                line: index + 1,
                file: None,
            },
            body: body.to_owned(),
            wrapper: Wrapper::JsonLog,
        });
    }
    (!records.is_empty()).then_some(records)
}

/// The columns of a csvlog record that explainsql reads, the same in every
/// version that has them.
const CSV_TIMESTAMP: usize = 0;
const CSV_USER: usize = 1;
const CSV_DATABASE: usize = 2;
const CSV_PID: usize = 3;
const CSV_MESSAGE: usize = 13;
const CSV_APPLICATION: usize = 22;
/// From PostgreSQL 14.
const CSV_QUERY_ID: usize = 25;

fn csvlog(text: &str) -> Option<Vec<Record>> {
    if !normalize::starts_log_entry(text) {
        return None;
    }
    let mut records = Vec::new();
    for (line, record) in normalize::csv_records_at(text) {
        if record.len() <= CSV_MESSAGE || !normalize::starts_log_entry(&record[CSV_TIMESTAMP]) {
            continue;
        }
        let message = &record[CSV_MESSAGE];
        let Some(body) = normalize::auto_explain_body(message) else {
            continue;
        };
        let field = |index: usize| record.get(index).filter(|value| !value.is_empty()).cloned();
        records.push(Record {
            meta: LogMeta {
                timestamp: field(CSV_TIMESTAMP),
                pid: field(CSV_PID).and_then(|pid| pid.parse().ok()),
                user: field(CSV_USER),
                database: field(CSV_DATABASE),
                application: field(CSV_APPLICATION),
                duration_us: duration(message),
                query_id: field(CSV_QUERY_ID)
                    .and_then(|id| id.parse().ok())
                    .filter(|&id: &i64| id != 0),
                line,
                file: None,
            },
            body: body.to_owned(),
            wrapper: Wrapper::CsvLog,
        });
    }
    (!records.is_empty()).then_some(records)
}

fn stderr(text: &str) -> Option<Vec<Record>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::new();
    for (index, header) in lines.iter().enumerate() {
        if !(header.ends_with("plan:") && header.contains("duration: ")) {
            continue;
        }
        // Continuation lines of a log message start with a tab. If the tabs
        // were lost on the way, read until the next line that starts a log
        // entry.
        let tabbed = lines
            .get(index + 1)
            .is_some_and(|line| line.starts_with('\t'));
        let mut body = Vec::new();
        for line in &lines[index + 1..] {
            if tabbed {
                match line.strip_prefix('\t') {
                    Some(rest) => body.push(rest),
                    None => break,
                }
            } else if normalize::starts_log_entry(line) {
                break;
            } else {
                body.push(line);
            }
        }
        let mut meta = prefix(header);
        meta.duration_us = duration(header);
        meta.line = index + 1;
        records.push(Record {
            meta,
            body: body.join("\n"),
            wrapper: Wrapper::AutoExplainLog,
        });
    }
    (!records.is_empty()).then_some(records)
}

/// What a stderr log's line prefix says: the time (`%m`, `%t`), the process
/// (`[%p]`), the user and database (`%u@%d`, or `user=%u,db=%d`) and the
/// application (`app=%a`). Other prefixes give what they give.
fn prefix(header: &str) -> LogMeta {
    let prefix = header.find("LOG:").map_or(header, |end| &header[..end]);
    let mut meta = LogMeta::default();
    let tokens: Vec<&str> = prefix.split_whitespace().collect();
    if normalize::starts_log_entry(prefix) && tokens.len() >= 2 {
        let mut stamp = vec![tokens[0], tokens[1]];
        if let Some(zone) = tokens.get(2).filter(|token| is_zone(token)) {
            stamp.push(zone);
        }
        meta.timestamp = Some(stamp.join(" "));
    }
    for token in &tokens {
        if let Some(pid) = token
            .find('[')
            .map(|open| &token[open + 1..])
            .and_then(|rest| rest.split(']').next())
            .and_then(|digits| digits.parse().ok())
        {
            meta.pid.get_or_insert(pid);
            continue;
        }
        for pair in token.split(',') {
            match pair.split_once('=') {
                Some(("user", value)) if !value.is_empty() => meta.user = Some(value.to_owned()),
                Some(("db", value)) if !value.is_empty() => meta.database = Some(value.to_owned()),
                Some(("app", value)) if !value.is_empty() && value != "[unknown]" => {
                    meta.application = Some(value.to_owned());
                }
                Some(_) => {}
                None => {
                    if let Some((user, database)) = pair.split_once('@') {
                        if !user.is_empty() && !database.is_empty() && meta.user.is_none() {
                            meta.user = Some(user.to_owned());
                            meta.database = Some(database.to_owned());
                        }
                    }
                }
            }
        }
    }
    meta
}

/// A time zone as `%m` and `%t` print it: `UTC`, `CEST`, `+03`, `-05:30`.
fn is_zone(token: &str) -> bool {
    token.chars().all(|c| c.is_ascii_uppercase())
        || (token.starts_with(['+', '-'])
            && token.len() > 1
            && token[1..].chars().all(|c| c.is_ascii_digit() || c == ':'))
}

/// `duration: 13.048 ms` in thousandths of a millisecond.
fn duration(message: &str) -> Option<u64> {
    let rest = &message[message.find("duration: ")? + "duration: ".len()..];
    let number = rest.split_whitespace().next()?;
    let ms: f64 = number.parse().ok()?;
    rest[number.len()..]
        .trim_start()
        .starts_with("ms")
        .then(|| (ms * 1000.0).round())
        .filter(|us| us.is_finite() && *us >= 0.0)
        .map(|us| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let us = us as u64;
            us
        })
}

/// The plan of an entry the log holds, its query text and parameters.
pub(super) fn entry(record: Record) -> Result<LogEntry, super::ParseError> {
    let (text, query, parameters) = normalize::split_entry(&record.body);
    let mut plan = super::parse_unwrapped(&text, vec![record.wrapper])?;
    if plan.summary.query_text.is_none() {
        plan.summary.query_text = query;
    }
    // JSON plans carry the parameters inside.
    let parameters = parameters.or_else(|| {
        plan.summary
            .extra
            .remove("Query Parameters")
            .and_then(|value| value.as_str().map(str::to_owned))
    });
    Ok(LogEntry {
        meta: record.meta,
        parameters,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_out_a_record_cut_short() {
        let record = r#"{"timestamp":"2026-10-06 06:35:13.475 UTC","pid":7,"message":"duration: 0.084 ms  plan:\nQuery Text: SELECT 1\nResult  (cost=0.00..0.01 rows=1 width=4)"}"#;
        let text = format!("{record}\n{record}\n{}", &record[..60]);
        let records = jsonlog(&text).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].meta.line, 2);
        // Not a jsonlog when the first line is not a record.
        assert!(jsonlog(&format!("text\n{record}")).is_none());
    }

    #[test]
    fn reads_the_line_prefix() {
        let meta = prefix(
            "2026-10-06 06:35:13.475 UTC [31577] postgres@shop LOG:  duration: 0.084 ms  plan:",
        );
        assert_eq!(
            meta.timestamp.as_deref(),
            Some("2026-10-06 06:35:13.475 UTC")
        );
        assert_eq!(meta.pid, Some(31577));
        assert_eq!(
            (meta.user.as_deref(), meta.database.as_deref()),
            (Some("postgres"), Some("shop"))
        );
        // As pgBadger recommends it.
        let meta = prefix(
            "2026-10-06 06:35:13 +03 [31577]: [3-1] user=app,db=shop,app=shop-api,client=10.0.0.7 LOG:  duration: 1.5 ms  plan:",
        );
        assert_eq!(meta.timestamp.as_deref(), Some("2026-10-06 06:35:13 +03"));
        assert_eq!(meta.pid, Some(31577));
        assert_eq!(meta.application.as_deref(), Some("shop-api"));
        assert_eq!(meta.user.as_deref(), Some("app"));
        assert_eq!(duration("duration: 13.048 ms  plan:"), Some(13_048));
        assert_eq!(duration("duration: 2 s"), None);
    }
}
