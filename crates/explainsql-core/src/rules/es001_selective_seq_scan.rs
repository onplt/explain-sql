//! ES001: a sequential scan of a large table that keeps few of the rows it
//! reads, on the hot path.
//!
//! The rows kept are those that pass the scan's filter or, for the inner
//! side of a nested loop, the loop's join filter. Silent when the table is
//! small, the scan is cheap compared with the statement, a `Limit` or a semi
//! join can stop it early, or the filter ORs conditions on different columns
//! (no single index serves it). When the filter wraps the column in a cast or
//! a function, the action is to rewrite the condition rather than to index
//! the column.

use super::predicate::{self, Access};
use super::{Context, Finding, Rule, Severity, column_list, evidence, qualifier, relation};
use crate::format;
use crate::ir::{Node, PredicateKind, Relationship};
use crate::metrics;

pub const RULE: Rule = Rule {
    id: "ES001",
    name: "Selective sequential scan",
};

/// Rows kept per row read, below which a scan is selective.
const MAX_SELECTIVITY: f64 = 0.05;
/// Pages read per scan, from which a table is large.
const MIN_PAGES: f64 = 1000.0;
/// Rows read per scan, from which a table is large when there are no buffer
/// counts.
const MIN_ROWS: f64 = 50_000.0;
/// Share of the runtime from which a scan is on the hot path.
const MIN_SHARE: f64 = 0.1;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type == "Seq Scan")
        .filter_map(|node| selective_scan(context, node))
        .collect()
}

fn selective_scan(context: &Context, node: &Node) -> Option<Finding> {
    let actuals = node.actuals.filter(|actuals| !actuals.never_executed())?;
    let metrics = context.metrics(node);
    if metrics.may_stop_early || !context.on_hot_path(node, MIN_SHARE) {
        return None;
    }
    let loops = actuals.loops as f64;
    let read = (actuals.rows + node.rows_removed_by_filter) * loops;
    let join = join_filter_above(context, node);
    let (kept, condition) = match join {
        Some((join, filter)) => (join.actuals?.rows * join.actuals?.loops as f64, filter),
        None => (actuals.rows * loops, node.predicate(PredicateKind::Filter)?),
    };
    if read <= 0.0 || kept / read >= MAX_SELECTIVITY {
        return None;
    }
    // Each loop scans the whole table; parallel processes share one scan.
    let scans = loops / metrics.processes;
    let pages = node
        .buffers
        .map(|buffers| metrics::blocks(&buffers) as f64 / scans);
    let large = match pages {
        Some(pages) => pages >= MIN_PAGES,
        None => read / scans >= MIN_ROWS,
    };
    if !large {
        return None;
    }
    let action = action(node, condition, join.is_some())?;

    let label = format::node(node);
    let summary = match join {
        Some(_) => format!(
            "{label} reads {} rows over {} loops, and the join filter above keeps {} of them",
            format::rows(read),
            format::rows(loops),
            format::rows(kept)
        ),
        None => format!(
            "{label} reads {} rows to keep {}",
            format::rows(read),
            format::rows(kept)
        ),
    };
    let mut facts = vec![
        evidence(
            "Rows kept",
            format!(
                "{} of {} ({})",
                format::rows(kept),
                format::rows(read),
                format::percent(kept / read)
            ),
        ),
        evidence(
            if join.is_some() {
                "Join filter"
            } else {
                "Filter"
            },
            condition,
        ),
    ];
    if let Some(pages) = pages {
        facts.push(evidence("Table size", format::pages(pages)));
    }
    if loops > 1.0 && metrics.processes == 1.0 {
        facts.push(evidence("Loops", format::rows(loops)));
    }
    facts.extend(context.time_evidence(node));
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(context.share(node)),
        node: Some(node.id),
        summary,
        evidence: facts,
        action,
    })
}

/// The nested loop and its join filter, when the scan is the loop's inner
/// side and the filter decides which of its rows are used.
fn join_filter_above<'a>(context: &Context<'a>, node: &Node) -> Option<(&'a Node, &'a str)> {
    if node.relationship != Some(Relationship::Inner) {
        return None;
    }
    let parent = context.parent(node)?;
    if parent.node_type != "Nested Loop" {
        return None;
    }
    Some((parent, parent.predicate(PredicateKind::JoinFilter)?))
}

/// What to suggest, or `None` when no index can serve the condition.
fn action(node: &Node, condition: &str, join: bool) -> Option<String> {
    let table = relation(node);
    let own = qualifier(node);
    let mut indexable = Vec::new();
    let mut wrapped = None;
    let mut operators = Vec::new();
    for conjunct in predicate::conjuncts(condition) {
        match predicate::access(conjunct) {
            Access::Column {
                column,
                operator,
                value,
            } => {
                indexable.push(column);
                operators.push((operator, value));
            }
            Access::Columns(a, b) if join => {
                // The join key on this scan's side.
                for column in [a, b] {
                    if predicate::split_column(column).0 == own {
                        indexable.push(column);
                    }
                }
            }
            Access::Wrapped { column, wrapper } => wrapped = wrapped.or(Some((column, wrapper))),
            _ => {}
        }
    }

    if !indexable.is_empty() {
        let columns = column_list(indexable.iter().copied());
        let kind = operators.iter().find_map(|&(operator, value)| match operator {
            "~~" | "~~*" if value.starts_with("'%") => Some(
                "Only a trigram index can serve a pattern that starts with %: CREATE EXTENSION pg_trgm, then a GIN index with gin_trgm_ops",
            ),
            "~~" => Some("For LIKE prefix searches, a b-tree index needs text_pattern_ops unless the column uses the C collation"),
            "@>" | "<@" | "?" | "?|" | "?&" | "&&" | "@@" => Some("This operator needs a GIN index"),
            _ => None,
        });
        let mut action = if join {
            format!(
                "An index on {table} ({columns}) would turn each loop's scan into an index lookup."
            )
        } else {
            format!(
                "An index on {table} ({columns}) would let PostgreSQL read only the matching rows."
            )
        };
        if let Some(kind) = kind {
            action.push(' ');
            action.push_str(kind);
            action.push('.');
        }
        return Some(action);
    }
    if let Some((column, wrapper)) = wrapped {
        let (_, name) = predicate::split_column(column);
        return Some(format!(
            "The filter applies {} to {name}, so an index on {name} cannot serve it. Rewrite the condition so that {name} stands alone, or index the expression itself.",
            if wrapper == "a cast" {
                "a cast".to_owned()
            } else {
                format!("{wrapper}()")
            }
        ));
    }
    // ORs across columns, or nothing recognizable: no single index to name.
    let unknown = predicate::conjuncts(condition)
        .into_iter()
        .any(|conjunct| predicate::access(conjunct) == Access::Unknown);
    unknown.then(|| format!("An index on {table} matching the filter would let PostgreSQL read only the matching rows."))
}
