//! The index advisor: turns the findings and a few plan patterns into
//! `CREATE INDEX CONCURRENTLY` candidates, query rewrites, and explanations
//! of why a slow scan gets no index.
//!
//! Precision comes first. Candidates come from the rules that already
//! decided a scan is worth indexing (ES001, ES005, ES006, ES009), plus three
//! patterns no rule covers: a top-N sort over a whole table, selective scans
//! of many partitions, and a correlated subquery that rescans a table. Each
//! candidate orders its columns by the ESR rule (equality, then sort, then
//! one range) and says how sure it is and what it could not check. Without
//! a database connection, nothing is verified.

mod keys;
mod patterns;

use serde::Serialize;

use crate::format;
use crate::ir::{NodeId, Plan};
use crate::metrics::Metrics;
use crate::rules::{Evidence, Finding};

/// What the advisor suggests for one part of the plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Advice {
    /// The node the advice is about: the scan an index would replace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    #[serde(flatten)]
    pub kind: AdviceKind,
    pub confidence: Confidence,
    /// Why, in one sentence.
    pub summary: String,
    pub evidence: Vec<Evidence>,
    /// What the suggestion could not take into account.
    pub caveats: Vec<String>,
    pub verification: Verification,
    /// The rules whose findings led here, if any.
    pub rules: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdviceKind {
    /// Create an index.
    Index { index: Index, ddl: String },
    /// Index the referencing columns of a foreign key, which the plan does
    /// not name.
    ForeignKey { constraint: String },
    /// Rewrite the condition: it wraps the column, so no index on the column
    /// can serve it.
    Rewrite { column: String, wrapper: String },
    /// A slow scan that an index would not help, and why.
    NoIndex { reason: String },
}

/// An index to create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Index {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub table: String,
    pub method: Method,
    pub columns: Vec<IndexColumn>,
    /// The table is partitioned, and its name was inferred from its
    /// partitions.
    pub partitioned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexColumn {
    /// As the plan prints it, quoted when needed.
    pub name: String,
    pub descending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opclass: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Btree,
    Gin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// How far a suggestion has been checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verification {
    /// From the plan alone.
    Unverified,
    /// The planner used a hypothetical index (HypoPG).
    Estimated,
    /// Measured with the index created in a rolled-back transaction.
    Measured,
}

impl Index {
    /// `orders (customer_id)`, `events USING gin (payload)`: the table and
    /// the columns, for display and tests.
    pub fn target(&self) -> String {
        let columns = self.column_list();
        match self.method {
            Method::Btree => format!("{} ({columns})", self.table),
            Method::Gin => format!("{} USING gin ({columns})", self.table),
        }
    }

    /// The statement that creates the index. `CONCURRENTLY` does not block
    /// writes, but PostgreSQL does not support it on partitioned tables.
    pub fn ddl(&self) -> String {
        let table = match &self.schema {
            Some(schema) => format!("{}.{}", quote(schema), quote(&self.table)),
            None => quote(&self.table),
        };
        let concurrently = if self.partitioned {
            ""
        } else {
            " CONCURRENTLY"
        };
        let method = match self.method {
            Method::Btree => "",
            Method::Gin => " USING gin",
        };
        format!(
            "CREATE INDEX{concurrently} ON {table}{method} ({});",
            self.column_list()
        )
    }

    fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|column| {
                let mut text = column.name.clone();
                if let Some(opclass) = column.opclass {
                    text.push(' ');
                    text.push_str(opclass);
                }
                if column.descending {
                    text.push_str(" DESC");
                }
                text
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether this index serves every query the other one serves: same
    /// table and method, and the other's columns are a prefix of these.
    fn covers(&self, other: &Index) -> bool {
        self.table == other.table
            && self.method == other.method
            && other.columns.len() <= self.columns.len()
            && self.columns.iter().zip(&other.columns).all(|(a, b)| a == b)
    }
}

impl Advice {
    /// The index, for index advice.
    pub fn index(&self) -> Option<&Index> {
        match &self.kind {
            AdviceKind::Index { index, .. } => Some(index),
            _ => None,
        }
    }

    /// A short title: `orders (customer_id)`, `Rewrite the condition on
    /// created_at`.
    pub fn title(&self) -> String {
        match &self.kind {
            AdviceKind::Index { index, .. } => format!("Index {}", index.target()),
            AdviceKind::ForeignKey { constraint } => {
                format!("Index the referencing columns of {constraint}")
            }
            AdviceKind::Rewrite { column, .. } => {
                format!("Rewrite the condition on {column}")
            }
            AdviceKind::NoIndex { .. } => "No index".to_owned(),
        }
    }
}

/// Quotes an identifier when PostgreSQL would need it.
fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if plain || name.starts_with('"') {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// The advice for a plan, given its metrics and findings: index candidates
/// (most confident first), then rewrites and foreign keys, then the slow
/// scans no index would help.
pub fn advise(plan: &Plan, metrics: &Metrics, findings: &[Finding]) -> Vec<Advice> {
    let context = patterns::Context { plan, metrics };
    let mut advice = Vec::new();
    for finding in findings {
        advice.extend(patterns::from_finding(&context, finding));
    }
    advice.extend(patterns::top_n(&context));
    advice.extend(patterns::partitions(&context, &advice));
    advice.extend(patterns::correlated_subqueries(&context));
    let mut advice = merge(advice);
    advice.extend(patterns::explanations(&context, &advice));
    advice.sort_by_key(|advice| {
        let rank = match advice.kind {
            AdviceKind::Index { .. } => 0,
            AdviceKind::Rewrite { .. } | AdviceKind::ForeignKey { .. } => 1,
            AdviceKind::NoIndex { .. } => 2,
        };
        (rank, std::cmp::Reverse(advice.confidence))
    });
    advice
}

/// Merges duplicates: the same index found twice, an index another one
/// covers, the same rewrite twice.
fn merge(advice: Vec<Advice>) -> Vec<Advice> {
    let mut merged: Vec<Advice> = Vec::new();
    for item in advice {
        let existing = merged
            .iter()
            .position(|other| match (&item.kind, &other.kind) {
                (AdviceKind::Index { index: a, .. }, AdviceKind::Index { index: b, .. }) => {
                    a.covers(b) || b.covers(a)
                }
                (a, b) => a == b,
            });
        let Some(position) = existing else {
            merged.push(item);
            continue;
        };
        let other = &mut merged[position];
        let wider = match (item.index(), other.index()) {
            (Some(a), Some(b)) => a.columns.len() > b.columns.len(),
            _ => false,
        };
        let (mut keep, absorbed) = if wider {
            (item, std::mem::replace(other, placeholder()))
        } else {
            (std::mem::replace(other, placeholder()), item)
        };
        keep.confidence = keep.confidence.max(absorbed.confidence);
        for rule in absorbed.rules {
            if !keep.rules.contains(&rule) {
                keep.rules.push(rule);
            }
        }
        for evidence in absorbed.evidence {
            if !keep.evidence.iter().any(|e| e.label == evidence.label) {
                keep.evidence.push(evidence);
            }
        }
        merged[position] = keep;
    }
    merged
}

fn placeholder() -> Advice {
    Advice {
        node: None,
        kind: AdviceKind::NoIndex {
            reason: String::new(),
        },
        confidence: Confidence::Low,
        summary: String::new(),
        evidence: Vec::new(),
        caveats: Vec::new(),
        verification: Verification::Unverified,
        rules: Vec::new(),
    }
}

/// The caveat every suggestion made from a plan alone carries.
const NOT_CONNECTED: &str = "Not connected to the database: existing indexes, the table's write load and the column statistics were not checked.";

/// Evidence for a scan's selectivity: `10 of 200,000 (0.005%)`.
fn kept_evidence(kept: f64, read: f64) -> Evidence {
    Evidence {
        label: "Rows kept",
        value: format!(
            "{} of {} ({})",
            format::rows(kept),
            format::rows(read),
            format::percent(if read > 0.0 { kept / read } else { 0.0 })
        ),
    }
}
