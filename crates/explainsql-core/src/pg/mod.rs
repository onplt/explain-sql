//! PostgreSQL plans: input normalization, the JSON and text parsers, and
//! lowering into the [IR](crate::ir).
//!
//! ```text
//! input ─▶ normalize ─▶ json | text ─▶ raw tree ─▶ lower ─▶ Plan
//! ```

mod json;
mod log;
mod lower;
mod normalize;
mod raw;
mod text;

use std::fmt;

use crate::ir::{Format, Plan, Source, Warning, Wrapper};

pub use log::{LogEntry, LogMeta};
pub(crate) use normalize::normalize_all;

/// Why an input could not be read as a plan at all. Smaller problems are
/// reported as [warnings](crate::ir::Plan::warnings) on a usable plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The input is empty or contains only whitespace.
    Empty,
    /// The input looks like JSON but cannot be read as an EXPLAIN plan.
    InvalidJson(String),
    /// The input does not look like an EXPLAIN plan.
    NoPlan,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => f.write_str("the input is empty"),
            ParseError::InvalidJson(reason) => write!(f, "not a JSON EXPLAIN plan: {reason}"),
            ParseError::NoPlan => f.write_str("no EXPLAIN plan found in the input"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses PostgreSQL `EXPLAIN` output in JSON or text format.
///
/// The input may still be wrapped the way it was copied: psql's table
/// output, an auto_explain entry from a server log (stderr or jsonlog), or a
/// Markdown code fence. Such wrappers are removed first and recorded in
/// [`Source::wrappers`].
pub fn parse(input: &str) -> Result<Plan, ParseError> {
    let normalized = normalize::normalize(input);
    if normalized.text.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut warnings = normalized.warnings;
    let raw = match normalized.format {
        Format::Json => {
            json::parse(&normalized.text, &mut warnings).map_err(ParseError::InvalidJson)?
        }
        Format::Text => text::parse(&normalized.text, &mut warnings).ok_or(ParseError::NoPlan)?,
    };
    let source = Source {
        format: normalized.format,
        wrappers: normalized.wrappers,
    };
    let mut plan = lower::lower(raw, source, warnings);
    if plan.summary.query_text.is_none() {
        plan.summary.query_text = normalized.query_text;
    }
    Ok(plan)
}

/// Parses every plan in the input, in order: plans pasted one after the
/// other, the plans of a JSON array, each auto_explain entry of a log, each
/// Markdown code fence and each table psql printed. Parts that hold no plan
/// are skipped; an error means no part held one.
///
/// For an input with a single plan, the result is that of [`parse`].
pub fn parse_all(input: &str) -> Result<Vec<Plan>, ParseError> {
    let mut plans = Vec::new();
    let mut error = None;
    for part in normalize::normalize_all(input) {
        if part.text.trim().is_empty() {
            continue;
        }
        let raws = match part.format {
            Format::Json => match json::parse_all(&part.text) {
                Ok(raws) => raws,
                Err(reason) => {
                    error.get_or_insert(ParseError::InvalidJson(reason));
                    continue;
                }
            },
            Format::Text => text::parse_all(&part.text),
        };
        for (index, (raw, warnings)) in raws.into_iter().enumerate() {
            // What the normalizer left out came before the part's first plan.
            let mut all = if index == 0 {
                part.warnings.clone()
            } else {
                Vec::new()
            };
            all.extend(warnings);
            let source = Source {
                format: part.format,
                wrappers: part.wrappers.clone(),
            };
            let mut plan = lower::lower(raw, source, all);
            if plan.summary.query_text.is_none() {
                plan.summary.query_text.clone_from(&part.query_text);
            }
            plans.push(plan);
        }
    }
    if plans.is_empty() {
        return Err(error.unwrap_or(if input.trim().is_empty() {
            ParseError::Empty
        } else {
            ParseError::NoPlan
        }));
    }
    Ok(plans)
}

/// Reads every auto_explain entry of a server log, in the log's order: a
/// jsonlog, a csvlog, or a stderr log with any line prefix, its plans in
/// JSON or text. An entry whose plan cannot be read is left out, with a
/// warning on its line.
pub fn parse_log(input: &str) -> Result<(Vec<LogEntry>, Vec<Warning>), ParseError> {
    let text = normalize::clean(input);
    if text.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let records = log::records(&text).ok_or(ParseError::NoPlan)?;
    let mut entries = Vec::with_capacity(records.len());
    let mut skipped = Vec::new();
    for record in records {
        let line = record.meta.line;
        match log::entry(record) {
            Ok(entry) => entries.push(entry),
            Err(error) => skipped.push(Warning {
                line: Some(line),
                message: format!("an auto_explain entry could not be read: {error}"),
            }),
        }
    }
    if entries.is_empty() {
        return Err(ParseError::NoPlan);
    }
    Ok((entries, skipped))
}

/// Parses the text of a plan that no wrapper surrounds any more.
fn parse_unwrapped(text: &str, wrappers: Vec<Wrapper>) -> Result<Plan, ParseError> {
    if text.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut warnings = Vec::new();
    let (raw, format) = if text.trim_start().starts_with(['[', '{']) {
        let raw = json::parse(text, &mut warnings).map_err(ParseError::InvalidJson)?;
        (raw, Format::Json)
    } else {
        let text = normalize::start_at_plan(text, &mut warnings);
        let raw = text::parse(&text, &mut warnings).ok_or(ParseError::NoPlan)?;
        (raw, Format::Text)
    };
    let plan = lower::lower(raw, Source { format, wrappers }, warnings);
    if plan.nodes.is_empty() {
        return Err(ParseError::NoPlan);
    }
    Ok(plan)
}
