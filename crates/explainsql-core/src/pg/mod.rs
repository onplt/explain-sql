//! PostgreSQL plans: input normalization, the JSON and text parsers, and
//! lowering into the [IR](crate::ir).
//!
//! ```text
//! input ─▶ normalize ─▶ json | text ─▶ raw tree ─▶ lower ─▶ Plan
//! ```

mod json;
mod lower;
mod normalize;
mod raw;
mod text;

use std::fmt;

use crate::ir::{Format, Plan, Source};

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
