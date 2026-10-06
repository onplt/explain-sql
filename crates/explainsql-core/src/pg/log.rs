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
    /// The session (`%c`): the process's start time and id, in hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The virtual transaction (`%v`), when one was open: `3/12`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vxid: Option<String>,
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
                session: field("session_id"),
                vxid: field("vxid").and_then(transaction),
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
const CSV_SESSION: usize = 5;
const CSV_VXID: usize = 9;
const CSV_MESSAGE: usize = 13;
const CSV_DETAIL: usize = 14;
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
                session: field(CSV_SESSION),
                vxid: field(CSV_VXID).and_then(transaction),
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
/// (`[%p]`), the user and database (`%u@%d`, or `user=%u,db=%d`), the
/// application (`app=%a`), the session (`%c`) and the virtual transaction
/// (`%v`). Other prefixes give what they give.
fn prefix(header: &str) -> LogMeta {
    let prefix = severity(header).map_or(header, |(at, _)| &header[..at]);
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
            let value = pair.split_once('=').map_or(pair, |(_, value)| value);
            if is_session(value) {
                meta.session.get_or_insert_with(|| value.to_owned());
                continue;
            }
            if is_vxid(value) {
                if meta.vxid.is_none() {
                    meta.vxid = transaction(value.to_owned());
                }
                continue;
            }
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

/// Where a stderr line's message starts: the position of its severity and
/// the message after it.
fn severity(line: &str) -> Option<(usize, &str)> {
    [
        "LOG:  ",
        "DETAIL:  ",
        "ERROR:  ",
        "STATEMENT:  ",
        "CONTEXT:  ",
        "HINT:  ",
    ]
    .iter()
    .filter_map(|marker| line.find(marker).map(|at| (at, &line[at + marker.len()..])))
    .min_by_key(|(at, _)| *at)
}

/// A session id as `%c` prints it: the process's start time and its id in
/// hex, `6ac51bfc.151b`.
fn is_session(token: &str) -> bool {
    token.split_once('.').is_some_and(|(time, pid)| {
        time.len() == 8
            && (1..=8).contains(&pid.len())
            && time
                .chars()
                .chain(pid.chars())
                .all(|c| c.is_ascii_hexdigit())
    })
}

/// A virtual transaction id as `%v` prints it: `3/12`.
fn is_vxid(token: &str) -> bool {
    token.split_once('/').is_some_and(|(backend, local)| {
        !backend.is_empty()
            && !local.is_empty()
            && backend
                .chars()
                .chain(local.chars())
                .all(|c| c.is_ascii_digit())
    })
}

/// A virtual transaction id, unless it says no transaction was open (`3/0`,
/// as a statement that committed on its own logs it).
fn transaction(vxid: String) -> Option<String> {
    (!vxid.ends_with("/0") && !vxid.is_empty()).then_some(vxid)
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

/// A statement a server log says ran, as statement logging writes it:
/// `log_min_duration_statement` (`duration: 0.4 ms  execute <unnamed>: …`
/// with the values in a `DETAIL: parameters: $1 = '42'` line), or
/// `log_statement` with or without `log_duration`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoggedStatement {
    /// The time is when it ended; the duration adds up the parse, bind and
    /// execute steps of a statement sent with the extended query protocol.
    #[serde(flatten)]
    pub meta: LogMeta,
    /// As the client sent it: with `$1` parameters when it was prepared,
    /// else with its values written in.
    pub text: String,
    /// The name it was prepared under: none for a simple query and for the
    /// unnamed statement drivers use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepared: Option<String>,
    /// Its parameters' values, `$1` first, as the log quotes them; `None`
    /// for NULL.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Option<String>>,
}

/// What a line of statement logging says.
#[derive(Debug, PartialEq)]
enum Logged<'a> {
    /// A statement that ran: a simple query (`statement:`), or the
    /// execution of a prepared one (`execute <name>:`).
    Run {
        duration_us: Option<u64>,
        prepared: Option<&'a str>,
        text: &'a str,
    },
    /// The parse or bind step of a statement sent with the extended query
    /// protocol, which its execution follows.
    Step(u64),
    /// A duration on its own (`log_duration`), of the statement logged
    /// before it.
    Duration(u64),
}

fn logged(message: &str) -> Option<Logged<'_>> {
    let (duration_us, rest) = match message.strip_prefix("duration: ") {
        Some(after) => {
            let us = duration(message)?;
            let end = after.find(" ms")? + " ms".len();
            (Some(us), after[end..].trim_start())
        }
        None => (None, message),
    };
    if rest.is_empty() {
        return duration_us.map(Logged::Duration);
    }
    if let Some(text) = rest.strip_prefix("statement: ") {
        return Some(Logged::Run {
            duration_us,
            prepared: None,
            text,
        });
    }
    for (step, word) in [(false, "execute "), (true, "parse "), (true, "bind ")] {
        let Some((name, text)) = rest
            .strip_prefix(word)
            .and_then(|rest| rest.split_once(": "))
        else {
            continue;
        };
        if step {
            return duration_us.map(Logged::Step);
        }
        return Some(Logged::Run {
            duration_us,
            prepared: (name != "<unnamed>").then_some(name),
            text,
        });
    }
    None
}

/// The values of `parameters: $1 = '42', $2 = NULL`, as statement logging
/// and auto_explain (`Query Parameters:`) write them: `$1` first, quotes
/// taken off.
pub fn parameter_values(text: &str) -> Vec<Option<String>> {
    let text = text.trim();
    let text = ["parameters: ", "Query Parameters: "]
        .iter()
        .find_map(|label| {
            text.get(..label.len())
                .filter(|start| start.eq_ignore_ascii_case(label))
                .map(|_| &text[label.len()..])
        })
        .unwrap_or(text);
    let mut values: Vec<Option<String>> = Vec::new();
    let mut rest = text;
    while let Some((number, after)) = rest
        .strip_prefix('$')
        .and_then(|after| after.split_once(" = "))
    {
        let Ok(number) = number.parse::<usize>() else {
            break;
        };
        let (value, after) = if let Some(after) = after.strip_prefix("NULL") {
            (None, after)
        } else if after.starts_with('\'') {
            let length = crate::params::quoted(after, '\'', false);
            let quoted = &after[..length];
            let inner = quoted
                .strip_prefix('\'')
                .and_then(|inner| inner.strip_suffix('\''))
                .unwrap_or(&quoted[1..]);
            (Some(inner.replace("''", "'")), &after[length..])
        } else {
            break;
        };
        if number == 0 || number > 65_535 {
            break;
        }
        if values.len() < number {
            values.resize(number, None);
        }
        values[number - 1] = value;
        match after.strip_prefix(", ") {
            Some(next) => rest = next,
            None => break,
        }
    }
    values
}

/// A message of a log and its detail, with what the log says about it.
struct Message {
    meta: LogMeta,
    text: String,
    detail: Option<String>,
}

/// Every statement a server log says ran, in the log's order; `None` when
/// the text is not a server log with statement logging.
pub(super) fn statements(text: &str) -> Option<Vec<LoggedStatement>> {
    let messages = json_messages(text)
        .or_else(|| csv_messages(text))
        .unwrap_or_else(|| stderr_messages(text));
    let mut statements: Vec<LoggedStatement> = Vec::new();
    // Per session: the time of the parse and bind steps so far, and the
    // statement a duration on its own would be of.
    let mut steps: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut last: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for message in messages {
        let session = session_key(&message.meta);
        match logged(&message.text) {
            Some(Logged::Step(us)) => *steps.entry(session).or_default() += us,
            Some(Logged::Duration(us)) => {
                if let Some(&index) = last.get(&session) {
                    let statement: &mut LoggedStatement = &mut statements[index];
                    if statement.meta.duration_us.is_none() {
                        statement.meta.duration_us = Some(us);
                        statement.meta.timestamp.clone_from(&message.meta.timestamp);
                    }
                }
            }
            Some(Logged::Run {
                duration_us,
                prepared,
                text,
            }) => {
                let steps = steps.remove(&session).unwrap_or(0);
                let mut meta = message.meta;
                meta.duration_us = duration_us.map(|us| us + steps);
                last.insert(session, statements.len());
                statements.push(LoggedStatement {
                    meta,
                    text: text.trim().to_owned(),
                    prepared: prepared.map(str::to_owned),
                    parameters: message
                        .detail
                        .as_deref()
                        .map(parameter_values)
                        .unwrap_or_default(),
                });
            }
            None => {}
        }
    }
    (!statements.is_empty()).then_some(statements)
}

/// What tells a log's sessions apart: the session id, or else the process.
fn session_key(meta: &LogMeta) -> String {
    match (&meta.session, meta.pid) {
        (Some(session), _) => session.clone(),
        (None, Some(pid)) => format!("pid {pid}"),
        (None, None) => String::new(),
    }
}

fn json_messages(text: &str) -> Option<Vec<Message>> {
    let mut messages = Vec::new();
    let mut first = true;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(record)) => record,
            _ if first => return None,
            _ => continue,
        };
        first = false;
        let field = |key: &str| {
            record
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let Some(text) = field("message") else {
            continue;
        };
        messages.push(Message {
            meta: LogMeta {
                timestamp: field("timestamp"),
                pid: record
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok()),
                user: field("user"),
                database: field("dbname"),
                application: field("application_name"),
                session: field("session_id"),
                vxid: field("vxid").and_then(transaction),
                query_id: record
                    .get("query_id")
                    .and_then(Value::as_i64)
                    .filter(|&id| id != 0),
                line: index + 1,
                ..LogMeta::default()
            },
            text,
            detail: field("detail"),
        });
    }
    (!first).then_some(messages)
}

fn csv_messages(text: &str) -> Option<Vec<Message>> {
    if !normalize::starts_log_entry(text) {
        return None;
    }
    let records = normalize::csv_records_at(text);
    // A stderr log starts with a time too; a csvlog's records have its
    // columns.
    if !records
        .iter()
        .any(|(_, record)| record.len() > CSV_APPLICATION && record[CSV_PID].parse::<u32>().is_ok())
    {
        return None;
    }
    let mut messages = Vec::new();
    for (line, record) in records {
        if record.len() <= CSV_MESSAGE || !normalize::starts_log_entry(&record[CSV_TIMESTAMP]) {
            continue;
        }
        let field = |index: usize| record.get(index).filter(|value| !value.is_empty()).cloned();
        messages.push(Message {
            meta: LogMeta {
                timestamp: field(CSV_TIMESTAMP),
                pid: field(CSV_PID).and_then(|pid| pid.parse().ok()),
                user: field(CSV_USER),
                database: field(CSV_DATABASE),
                application: field(CSV_APPLICATION),
                session: field(CSV_SESSION),
                vxid: field(CSV_VXID).and_then(transaction),
                query_id: field(CSV_QUERY_ID)
                    .and_then(|id| id.parse().ok())
                    .filter(|&id: &i64| id != 0),
                line,
                ..LogMeta::default()
            },
            text: record[CSV_MESSAGE].clone(),
            detail: field(CSV_DETAIL),
        });
    }
    Some(messages)
}

/// The messages of a stderr log: each line with a severity, and the lines
/// that continue it, which start with a tab. A `DETAIL` line goes with the
/// message of its process before it.
fn stderr_messages(text: &str) -> Vec<Message> {
    let lines: Vec<&str> = text.lines().collect();
    let mut messages: Vec<Message> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let start = index;
        index += 1;
        let Some((at, first)) = severity(line).filter(|_| !line.starts_with('\t')) else {
            continue;
        };
        let mut text = first.to_owned();
        while let Some(rest) = lines.get(index).and_then(|line| line.strip_prefix('\t')) {
            text.push('\n');
            text.push_str(rest);
            index += 1;
        }
        let mut meta = prefix(line);
        meta.line = start + 1;
        let kind = &line[at..];
        if kind.starts_with("DETAIL:") {
            // The message it details: the last of its process.
            if let Some(message) = messages
                .iter_mut()
                .rev()
                .find(|message| message.meta.pid == meta.pid)
            {
                message.detail.get_or_insert(text);
            }
        } else {
            // Errors and other messages too, so that their DETAIL lines
            // are not taken for a statement's.
            messages.push(Message {
                meta,
                text,
                detail: None,
            });
        }
    }
    messages
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

    #[test]
    fn reads_statement_logging() {
        let read = |name: &str| {
            let path = format!(
                "{}/../../fixtures/requests/{name}",
                env!("CARGO_MANIFEST_DIR")
            );
            statements(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let stderr = read("postgresql.log");
        // The same statements, with the same values and sessions, in every
        // format.
        for other in [read("postgresql.csv"), read("postgresql.json")] {
            assert_eq!(other.len(), stderr.len());
            for (a, b) in stderr.iter().zip(&other) {
                assert_eq!(
                    (&a.text, &a.parameters, &a.meta.session, &a.meta.vxid),
                    (&b.text, &b.parameters, &b.meta.session, &b.meta.vxid)
                );
                assert_eq!(a.meta.duration_us, b.meta.duration_us);
                assert_eq!(a.meta.timestamp, b.meta.timestamp);
            }
        }
        let first = &stderr[0];
        assert!(
            first
                .text
                .starts_with("SELECT id, status, created_at FROM orders WHERE customer_id = $1")
        );
        assert_eq!(
            first.parameters,
            vec![Some("4242".to_owned()), Some("5".to_owned())]
        );
        // Parse, bind and execute.
        assert_eq!(first.meta.duration_us, Some(1_026 + 669 + 30_724));
        assert_eq!(first.meta.vxid.as_deref(), Some("2/2"));
        assert_eq!(first.meta.session.as_deref(), Some("6ac51bfc.151b"));
        assert_eq!(first.prepared, None);
        // A simple query that committed on its own: no transaction.
        let last = stderr.last().unwrap();
        assert_eq!(
            last.text,
            "SELECT id, email FROM customers WHERE id = 4242;"
        );
        assert_eq!(last.meta.vxid, None);
        assert!(last.parameters.is_empty());
    }

    #[test]
    fn reads_the_lines_of_statement_logging() {
        assert_eq!(
            logged("duration: 0.050 ms  execute S_1: SELECT 1"),
            Some(Logged::Run {
                duration_us: Some(50),
                prepared: Some("S_1"),
                text: "SELECT 1"
            })
        );
        assert_eq!(
            logged("statement: SELECT 2"),
            Some(Logged::Run {
                duration_us: None,
                prepared: None,
                text: "SELECT 2"
            })
        );
        assert_eq!(
            logged("duration: 0.2 ms  bind <unnamed>: SELECT 1"),
            Some(Logged::Step(200))
        );
        assert_eq!(logged("duration: 1.5 ms"), Some(Logged::Duration(1_500)));
        assert_eq!(logged("duration: 1.5 ms  plan:\nQuery Text: x"), None);
        assert_eq!(logged("checkpoint starting: time"), None);
        assert_eq!(
            parameter_values("parameters: $1 = 'it''s', $3 = NULL, $2 = '7'"),
            vec![Some("it's".to_owned()), Some("7".to_owned()), None]
        );
        assert_eq!(
            parameter_values("Query Parameters: $1 = '777'"),
            vec![Some("777".to_owned())]
        );
        assert!(parameter_values("parameters: none").is_empty());
    }

    #[test]
    fn reads_log_statement_with_log_duration() {
        let log = "2026-10-06 06:00:00.100 UTC [7] app@shop LOG:  statement: SELECT 1\n\
                   2026-10-06 06:00:00.105 UTC [8] app@shop LOG:  statement: SELECT\n\
                   \t2\n\
                   2026-10-06 06:00:00.110 UTC [7] app@shop LOG:  duration: 9.000 ms\n";
        let found = statements(log).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].meta.duration_us, Some(9_000));
        assert_eq!(
            found[0].meta.timestamp.as_deref(),
            Some("2026-10-06 06:00:00.110 UTC")
        );
        assert_eq!(found[1].text, "SELECT\n2");
        assert_eq!(found[1].meta.duration_us, None);
        assert_eq!(found[1].meta.line, 2);
    }

    #[test]
    fn reads_sessions_and_transactions_from_the_prefix() {
        let meta = prefix(
            "2026-10-06 16:04:12.294 UTC [5403] postgres@shop 6ac51bfc.151b 2/2 LOG:  duration: 1 ms  statement: x",
        );
        assert_eq!(meta.session.as_deref(), Some("6ac51bfc.151b"));
        assert_eq!(meta.vxid.as_deref(), Some("2/2"));
        let meta = prefix(
            "2026-10-06 16:04:12 UTC [5403] session=6ac51bfc.151b,vxid=2/0 DETAIL:  parameters: $1 = '1'",
        );
        assert_eq!(meta.session.as_deref(), Some("6ac51bfc.151b"));
        assert_eq!(meta.vxid, None);
        assert_eq!(meta.pid, Some(5403));
    }
}
