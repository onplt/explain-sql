//! Plans over time, from server logs: which plans each statement got, when
//! its plan changed, what changed, and how long it ran before and after.
//!
//! Statements are told apart by their query identifier (`compute_query_id`,
//! logged with auto_explain's `log_verbose`), or else by their text with
//! comments, literal values and parameters left out. sqlcommenter tags in
//! the text (`/*controller='OrderController',action='latest'*/`) say where
//! in the application a statement comes from. A plan that keeps a prepared
//! statement's parameters (`$1`) is its generic plan, which PostgreSQL may
//! switch to after five executions.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::diff;
use crate::fingerprint;
use crate::params;
use crate::pg::LogEntry;

/// The statements of a log and the plans they got.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Timeline {
    /// How many entries were read.
    pub entries: usize,
    /// The first and last timestamps, as the log prints them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Statements whose plan changed first, the costliest change first; then
    /// the others, the longest in all first.
    pub statements: Vec<Statement>,
}

/// One statement and the plans it got, in time order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Statement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_id: Option<i64>,
    /// The text of its first entry, without comments, literal values or
    /// parameters.
    pub text: String,
    /// The name it was prepared under, for `EXECUTE`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepared: Option<String>,
    pub runs: usize,
    /// Its logged durations summed, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    pub applications: Vec<String>,
    /// sqlcommenter tags, the trace context left out: `controller`,
    /// `action`, `framework`.
    pub tags: BTreeMap<String, String>,
    /// Each plan it got, in the order they first appear.
    pub plans: Vec<PlanUse>,
    pub changes: Vec<PlanChange>,
    pub pattern: Pattern,
    /// Its entries, in time order: indexes into the entries read.
    pub entries: Vec<usize>,
}

/// A plan a statement got.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanUse {
    /// Its [shape id](fingerprint::id).
    pub shape: String,
    /// How it reads its tables.
    pub access: String,
    pub runs: usize,
    /// The median of its logged durations, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    /// It keeps the statement's parameters: the generic plan.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub generic: bool,
    /// Its first entry.
    pub first: usize,
}

/// Where a statement's plan changed: from the plan of one entry to another
/// plan in the next.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanChange {
    /// The last entry with the plan before, and the first with the plan
    /// after.
    pub before: usize,
    pub after: usize,
    /// When the plan after first ran, as the log prints it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// The plans' shape ids.
    pub from: String,
    pub to: String,
    /// How many runs in a row each plan had, around the change.
    pub runs_before: usize,
    pub runs_after: usize,
    /// The median durations of those runs, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_before: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_after: Option<f64>,
    /// How the plan after compares, and its main change, as `explainsql
    /// diff` says it.
    pub verdict: String,
    /// The other changes, the most significant first, at most three.
    pub details: Vec<String>,
    /// The plan after is the generic plan, the plan before was not: a
    /// prepared statement switched to it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub generic: bool,
    /// The values the statement ran with in the first run of the plan
    /// after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    /// Another session ran the plan after.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pattern {
    /// One plan all along.
    Stable,
    /// Another plan from some point, and maybe back.
    Changed,
    /// Back and forth between plans, as values or sessions come: for a
    /// prepared statement, custom and generic plans; otherwise often a plan
    /// that depends on the values.
    Alternating,
}

impl Pattern {
    pub fn label(self) -> &'static str {
        match self {
            Pattern::Stable => "STABLE",
            Pattern::Changed => "CHANGED",
            Pattern::Alternating => "ALTERNATING",
        }
    }
}

/// How many changes of each listed for a statement whose plans alternate.
const ALTERNATING_FROM: usize = 4;
/// Changes described in detail, per change.
const DETAILS: usize = 3;

/// The statements of the entries and the plans they got.
pub fn timeline(entries: &[LogEntry]) -> Timeline {
    // Time order; entries without a time keep their place.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&index| (instant(entries[index].meta.timestamp.as_deref()), index));

    // Statements, by query identifier or by their text.
    let mut keys: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &index in &order {
        let key = key(&entries[index]);
        if !groups.contains_key(&key) {
            keys.push(key.clone());
        }
        groups.entry(key).or_default().push(index);
    }

    let mut statements: Vec<Statement> = keys
        .iter()
        .map(|key| statement(entries, &groups[key]))
        .collect();
    statements.sort_by(|a, b| {
        let changed = |statement: &Statement| statement.pattern != Pattern::Stable;
        changed(b)
            .cmp(&changed(a))
            .then_with(|| impact(b).total_cmp(&impact(a)))
    });

    let stamps: Vec<&str> = order
        .iter()
        .filter_map(|&index| entries[index].meta.timestamp.as_deref())
        .collect();
    Timeline {
        entries: entries.len(),
        from: stamps.first().map(|stamp| (*stamp).to_owned()),
        to: stamps.last().map(|stamp| (*stamp).to_owned()),
        statements,
    }
}

/// What tells an entry's statement from others: its query identifier, or
/// else its text without comments and values.
pub fn key(entry: &LogEntry) -> String {
    match query_id(entry) {
        Some(id) => format!("id:{id}"),
        None => format!(
            "text:{}",
            normalize(entry.plan.summary.query_text.as_deref().unwrap_or("")).to_lowercase()
        ),
    }
}

/// The query identifier the plan carries, or else the log's.
fn query_id(entry: &LogEntry) -> Option<i64> {
    entry
        .plan
        .summary
        .query_identifier
        .filter(|&id| id != 0)
        .or(entry.meta.query_id)
}

fn statement(entries: &[LogEntry], indexes: &[usize]) -> Statement {
    let first = &entries[indexes[0]];
    let shapes: Vec<String> = indexes
        .iter()
        .map(|&index| fingerprint::id(&entries[index].plan))
        .collect();
    let durations: Vec<Option<f64>> = indexes
        .iter()
        .map(|&index| entries[index].meta.duration())
        .collect();
    let parameterized = |entry: &LogEntry| {
        entry.parameters.is_some()
            || entry
                .plan
                .summary
                .query_text
                .as_deref()
                .is_some_and(|text| {
                    let found = params::placeholders(text);
                    found.count > 0 && !found.converted
                })
    };
    let generic = |entry: &LogEntry| parameterized(entry) && keeps_parameters(entry);

    // The plans, in the order they first appear.
    let mut plans: Vec<PlanUse> = Vec::new();
    for (position, shape) in shapes.iter().enumerate() {
        if plans.iter().any(|plan| &plan.shape == shape) {
            continue;
        }
        let runs: Vec<Option<f64>> = shapes
            .iter()
            .zip(&durations)
            .filter(|(other, _)| *other == shape)
            .map(|(_, duration)| *duration)
            .collect();
        let entry = &entries[indexes[position]];
        plans.push(PlanUse {
            shape: shape.clone(),
            access: params::brief(&entry.plan, &[]).access,
            runs: runs.len(),
            median: median(&runs),
            generic: generic(entry),
            first: indexes[position],
        });
    }

    // Runs of the same plan in a row, and the changes between them.
    let mut segments: Vec<(usize, usize)> = Vec::new();
    for position in 0..shapes.len() {
        match segments.last_mut() {
            Some((_, end)) if shapes[*end] == shapes[position] => *end = position,
            _ => segments.push((position, position)),
        }
    }
    let changes: Vec<PlanChange> = segments
        .windows(2)
        .map(|pair| {
            let ((start, end), (next, next_end)) = (pair[0], pair[1]);
            let (before, after) = (&entries[indexes[end]], &entries[indexes[next]]);
            let difference = diff::diff(&before.plan, &after.plan);
            // The changes the verdict does not tell already.
            let details: Vec<String> = difference
                .changes
                .iter()
                .filter(|change| !difference.verdict.contains(&change.summary))
                .take(DETAILS)
                .map(|change| format!("{} {}", change.kind.label(), change.summary))
                .collect();
            PlanChange {
                before: indexes[end],
                after: indexes[next],
                at: after.meta.timestamp.clone(),
                from: shapes[end].clone(),
                to: shapes[next].clone(),
                runs_before: end - start + 1,
                runs_after: next_end - next + 1,
                median_before: median(&durations[start..=end]),
                median_after: median(&durations[next..=next_end]),
                verdict: difference.verdict,
                details,
                generic: generic(after) && !generic(before),
                parameters: after.parameters.clone(),
                new_session: before.meta.pid.is_some() && before.meta.pid != after.meta.pid,
            }
        })
        .collect();
    let distinct = plans.len();
    let pattern = if segments.len() <= 1 {
        Pattern::Stable
    } else if segments.len() >= ALTERNATING_FROM && segments.len() > 2 * distinct {
        Pattern::Alternating
    } else {
        Pattern::Changed
    };

    let mut applications: Vec<String> = Vec::new();
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    for &index in indexes {
        let entry = &entries[index];
        if let Some(application) = &entry.meta.application {
            if !applications.contains(application) {
                applications.push(application.clone());
            }
        }
        if let Some(text) = &entry.plan.summary.query_text {
            for (key, value) in self::tags(text) {
                if key != "traceparent" && key != "tracestate" {
                    tags.entry(key).or_insert(value);
                }
            }
        }
    }
    let total: Option<f64> = durations.iter().flatten().copied().reduce(|a, b| a + b);

    Statement {
        query_id: query_id(first),
        text: normalize(first.plan.summary.query_text.as_deref().unwrap_or("")),
        prepared: first
            .plan
            .summary
            .query_text
            .as_deref()
            .and_then(prepared_name),
        runs: indexes.len(),
        total,
        applications,
        tags,
        plans,
        changes,
        pattern,
        entries: indexes.to_vec(),
    }
}

/// What a statement's changes cost: the time its plans after took over
/// those before, for the runs after; for a statement whose plan did not
/// change, its total time.
fn impact(statement: &Statement) -> f64 {
    let changes = statement
        .changes
        .iter()
        .filter_map(|change| {
            let (before, after) = (change.median_before?, change.median_after?);
            #[allow(clippy::cast_precision_loss)]
            Some((after - before) * change.runs_after as f64)
        })
        .fold(f64::NEG_INFINITY, f64::max);
    if changes.is_finite() {
        changes
    } else {
        statement.total.unwrap_or(0.0)
    }
}

/// Whether a plan keeps parameters (`$1`) in its conditions, as a generic
/// plan does; a custom plan has the values in their place.
fn keeps_parameters(entry: &LogEntry) -> bool {
    entry.plan.nodes.iter().any(|node| {
        node.predicates.iter().any(|predicate| {
            let found = params::placeholders(&predicate.text);
            found.count > 0 && !found.converted
        })
    })
}

fn median(durations: &[Option<f64>]) -> Option<f64> {
    let mut known: Vec<f64> = durations.iter().flatten().copied().collect();
    if known.is_empty() {
        return None;
    }
    known.sort_by(f64::total_cmp);
    let middle = known.len() / 2;
    Some(if known.len() % 2 == 0 {
        (known[middle - 1] + known[middle]) / 2.0
    } else {
        known[middle]
    })
}

/// A timestamp as the log prints it, as a number that orders: milliseconds
/// from the year 0, the time zone left out, as all of a log's entries
/// share it.
pub fn instant(timestamp: Option<&str>) -> Option<i64> {
    let timestamp = timestamp?;
    let (date, rest) = timestamp.split_once([' ', 'T']).unwrap_or((timestamp, ""));
    let mut parts = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    let time = rest.split_whitespace().next().unwrap_or("");
    let mut clock = time.split(':').filter(|part| !part.is_empty());
    let hours: i64 = clock.next().map_or(Some(0), |part| part.parse().ok())?;
    let minutes: i64 = clock.next().map_or(Some(0), |part| part.parse().ok())?;
    let seconds: f64 = clock.next().map_or(Some(0.0), |part| {
        part.trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()
    })?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era;
    #[allow(clippy::cast_possible_truncation)]
    let millis = (seconds * 1000.0).round() as i64;
    Some(((days * 24 + hours) * 60 + minutes) * 60_000 + millis)
}

/// A statement's text as statements are told apart: without comments and a
/// leading `PREPARE name (types) AS`, each literal value and parameter a
/// `?`, a list of them in `IN (…)` one `?`, its blanks collapsed and no
/// final semicolon.
pub fn normalize(sql: &str) -> String {
    let sql = strip_prepare(sql.trim());
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    let previous_word = |out: &String| {
        out.chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    };
    while let Some(c) = rest.chars().next() {
        let (length, replacement): (usize, Option<&str>) = if c == '\'' {
            // E'…' takes backslash escapes; the E goes with the literal.
            let escapes = out.ends_with(['E', 'e'])
                && !out[..out.len() - 1]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if escapes {
                out.pop();
            }
            (params::quoted(rest, '\'', escapes), Some("?"))
        } else if c == '"' {
            (params::quoted(rest, '"', false), None)
        } else if rest.starts_with("--") {
            (rest.find('\n').unwrap_or(rest.len()), Some(" "))
        } else if rest.starts_with("/*") {
            (params::block_comment(rest), Some(" "))
        } else if c == '$' {
            match params::dollar_quote(rest) {
                Some(end) => (end, Some("?")),
                None => {
                    let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
                    if digits > 0 && !previous_word(&out) {
                        (1 + digits, Some("?"))
                    } else {
                        (c.len_utf8(), None)
                    }
                }
            }
        } else if c.is_ascii_digit() && !previous_word(&out) {
            let length = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
                .unwrap_or(rest.len());
            (length, Some("?"))
        } else if c.is_whitespace() {
            let length = rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            (length, Some(" "))
        } else {
            (c.len_utf8(), None)
        };
        match replacement {
            Some(" ") => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            Some(text) => out.push_str(text),
            None => out.push_str(&rest[..length]),
        }
        rest = &rest[length..];
    }
    let out = collapse_lists(out.trim());
    out.trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_owned()
}

/// `IN (?, ?, ?)` as `IN (?)`.
fn collapse_lists(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('(') {
        out.push_str(&rest[..=at]);
        rest = &rest[at + 1..];
        let before = out[..out.len() - 1].trim_end().to_ascii_lowercase();
        if before.ends_with(" in") || before.ends_with("in") && before.len() == 2 {
            if let Some(close) = rest.find(')') {
                let inside = &rest[..close];
                if !inside.is_empty() && inside.split(',').all(|item| item.trim() == "?") {
                    out.push('?');
                    rest = &rest[close..];
                }
            }
        }
    }
    out.push_str(rest);
    out
}

/// `latest` for `PREPARE latest (integer) AS …`.
fn prepared_name(sql: &str) -> Option<String> {
    let sql = sql.trim_start();
    let rest = sql
        .get(..7)
        .filter(|word| word.eq_ignore_ascii_case("prepare"))
        .map(|_| sql[7..].trim_start())?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty() && strip_prepare(sql) != sql).then_some(name)
}

/// The text after a leading `PREPARE name [(types)] AS`.
fn strip_prepare(sql: &str) -> &str {
    let Some(rest) = sql
        .get(..7)
        .filter(|word| word.eq_ignore_ascii_case("prepare"))
        .map(|_| &sql[7..])
    else {
        return sql;
    };
    let lower = rest.to_ascii_lowercase();
    // The AS that ends the header: after the name and the types.
    let mut depth = 0i32;
    for (at, c) in lower.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0
                && lower[at..].starts_with("as")
                && lower[..at].ends_with(char::is_whitespace)
                && lower[at + 2..].starts_with(char::is_whitespace) =>
            {
                return rest[at + 2..].trim_start();
            }
            _ => {}
        }
    }
    sql
}

/// sqlcommenter's tags in a statement's comments, values decoded: those of
/// the last comment that holds nothing else.
pub fn tags(sql: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let mut rest = sql;
    while let Some(start) = rest.find("/*") {
        let length = params::block_comment(&rest[start..]);
        let comment = &rest[start..start + length];
        rest = &rest[start + length..];
        let inside = comment
            .strip_prefix("/*")
            .and_then(|comment| comment.strip_suffix("*/"))
            .unwrap_or("")
            .trim();
        if let Some(pairs) = comment_tags(inside) {
            found = pairs;
        }
    }
    found
}

/// `key='value',key='value'`, or `None` when the comment holds anything
/// else.
fn comment_tags(text: &str) -> Option<BTreeMap<String, String>> {
    let mut tags = BTreeMap::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (key, after) = rest.split_once("='")?;
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_alphanumeric() || "_-.".contains(c))
        {
            return None;
        }
        // The value ends at a quote not escaped with a backslash.
        let mut end = None;
        let mut escaped = false;
        for (at, c) in after.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '\'' if !escaped => {
                    end = Some(at);
                    break;
                }
                _ => escaped = false,
            }
        }
        let end = end?;
        tags.insert(
            percent_decode(&key.replace('\\', "")),
            percent_decode(&after[..end].replace("\\'", "'")),
        );
        rest = after[end + 1..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    (!tags.is_empty()).then_some(tags)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[at + 1..at + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The trace id of a W3C traceparent tag: `00-<trace id>-<span id>-<flags>`.
pub fn trace_id(traceparent: &str) -> Option<&str> {
    let mut parts = traceparent.split('-');
    let (_, trace) = (parts.next()?, parts.next()?);
    (trace.len() == 32 && trace.chars().all(|c| c.is_ascii_hexdigit())).then_some(trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_statements_apart_by_their_text() {
        assert_eq!(
            normalize(
                "SELECT id FROM orders WHERE customer_id = 4242 AND note = 'it''s' AND status IN ('a', 'b', $1) LIMIT 10 /*action='latest'*/;"
            ),
            "SELECT id FROM orders WHERE customer_id = ? AND note = ? AND status IN (?) LIMIT ?"
        );
        // A prepared statement is its query.
        assert_eq!(
            normalize(
                "PREPARE latest (integer, bigint) AS\n  SELECT id FROM orders WHERE customer_id = $1 LIMIT $2;"
            ),
            "SELECT id FROM orders WHERE customer_id = ? LIMIT ?"
        );
        // Identifiers with digits, quoted identifiers, escapes and dollar
        // quotes.
        assert_eq!(
            normalize("select t1.\"Col 2\" from t1 where x = E'a\\'b' -- note\n and y = $$z$$"),
            "select t1.\"Col 2\" from t1 where x = ? and y = ?"
        );
    }

    #[test]
    fn reads_sqlcommenter_tags() {
        let tags = tags(
            "SELECT 1 /*action='latest',controller='Order%20Controller',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'*/",
        );
        assert_eq!(tags["controller"], "Order Controller");
        assert_eq!(tags["action"], "latest");
        assert_eq!(
            trace_id(&tags["traceparent"]),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        // A comment that is not tags.
        assert!(super::tags("SELECT 1 /* just a note */").is_empty());
    }

    #[test]
    fn orders_timestamps() {
        let a = instant(Some("2026-10-06 06:35:13.475 UTC")).unwrap();
        let b = instant(Some("2026-10-06 06:35:13.598 UTC")).unwrap();
        assert_eq!(b - a, 123);
        let next_day = instant(Some("2026-10-07 00:00:00")).unwrap();
        let before = instant(Some("2026-10-06 23:59:59.999")).unwrap();
        assert_eq!(next_day - before, 1);
        let leap = instant(Some("2024-03-01")).unwrap() - instant(Some("2024-02-28")).unwrap();
        assert_eq!(leap, 2 * 86_400_000);
        assert_eq!(instant(Some("not a time")), None);
    }
}
