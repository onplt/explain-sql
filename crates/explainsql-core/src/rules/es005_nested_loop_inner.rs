//! ES005: a nested loop that spends most of its time repeating its inner
//! side (scanning it and filtering the pairs), where each repetition is a
//! sequential scan or a scan that throws away most of what it reads.
//!
//! Silent when the loop is cheap compared with the statement, when the inner
//! side runs once, and when a Materialize or Memoize caches the inner side.

use super::predicate::{self, Access};
use super::{
    Context, Finding, Rule, Severity, column_list, evidence, qualifier, relation, scan_below,
};
use crate::format;
use crate::ir::{Node, PredicateKind, Relationship};

pub const RULE: Rule = Rule {
    id: "ES005",
    name: "Expensive nested-loop inner side",
};

/// Share of the loop's time not spent producing outer rows.
const MIN_INNER_SHARE: f64 = 0.5;
/// Share of the runtime spent in the loop.
const MIN_SHARE: f64 = 0.1;
/// A scan that removes this many times the rows it keeps is heavily
/// filtered.
const MIN_REMOVED_RATIO: f64 = 10.0;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type == "Nested Loop")
        .filter_map(|node| expensive_inner(context, node))
        .collect()
}

fn expensive_inner(context: &Context, join: &Node) -> Option<Finding> {
    let inner = context.child(join, Relationship::Inner)?;
    let loops = inner.actuals?.loops;
    if loops < 2 {
        return None;
    }
    let join_time = context.metrics(join).inclusive_time?;
    let inner_time = context.metrics(inner).inclusive_time?;
    let outer_time = context
        .child(join, Relationship::Outer)
        .and_then(|outer| context.metrics(outer).inclusive_time)
        .unwrap_or(0.0);
    // The inner scans and the join filter run once per outer row.
    let repeated = join_time - outer_time;
    if repeated < MIN_INNER_SHARE * join_time
        || context
            .inclusive_share(join)
            .is_some_and(|share| share < MIN_SHARE)
    {
        return None;
    }
    let scan = scan_below(context, inner)?;
    let scan_actuals = scan.actuals?;
    let removed = scan.rows_removed_by_filter;
    let join_removed = join.rows_removed_by_join_filter;
    let heavily_filtered =
        removed >= MIN_REMOVED_RATIO * scan_actuals.rows.max(1.0) && removed >= 100.0;
    let sequential = scan.node_type == "Seq Scan";
    if !sequential && !heavily_filtered {
        return None;
    }

    let mut facts = vec![
        evidence("Inner loops", format::rows(loops as f64)),
        evidence(
            "Inner side per loop",
            format::duration(inner_time / loops as f64),
        ),
        evidence(
            "Repeated work",
            format!(
                "{} of the loop's {}, {} of it in the inner scans",
                format::duration(repeated),
                format::duration(join_time),
                format::duration(inner_time)
            ),
        ),
    ];
    if removed > 0.0 {
        facts.push(evidence(
            "Rows removed per loop",
            format!(
                "{} by the filter of {}",
                format::rows(removed),
                format::node(scan)
            ),
        ));
    } else if join_removed > 0.0 {
        facts.push(evidence(
            "Rows removed by the join filter",
            format::rows(join_removed),
        ));
    }

    let key = join_key(join, scan);
    let table = relation(scan);
    let mut action = match key {
        Some(columns) => format!(
            "An index on {table} ({columns}) would turn each of the {} inner scans into an index lookup.",
            format::rows(loops as f64)
        ),
        None => format!(
            "An index on {table} matching the join condition would turn each inner scan into an index lookup."
        ),
    };
    if let Some(outer) = context.child(join, Relationship::Outer) {
        if let (Some(error), Some(estimates)) =
            (context.metrics(outer).misestimate, outer.estimates)
        {
            if error.underestimated && error.factor >= 10.0 {
                action.push_str(&format!(
                    " The planner expected only {} outer rows (ES002); with the real count it would likely have chosen another join method.",
                    format::rows(estimates.rows)
                ));
            }
        }
    }
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(
            context
                .metrics
                .statement
                .total_time
                .map(|total| repeated / total),
        ),
        node: Some(join.id),
        summary: format!(
            "{} repeats {} {} times; that takes {} of the loop's time",
            format::node(join),
            format::node(scan),
            format::rows(loops as f64),
            format::percent(repeated / join_time.max(f64::MIN_POSITIVE)),
        ),
        evidence: facts,
        action,
    })
}

/// The inner scan's side of the join condition: from the loop's join filter
/// or from the scan's own conditions that refer to the outer side.
fn join_key(join: &Node, scan: &Node) -> Option<String> {
    let own = qualifier(scan);
    let mut columns = Vec::new();
    let conditions = join
        .predicate(PredicateKind::JoinFilter)
        .into_iter()
        .chain(scan.predicate(PredicateKind::Filter));
    for condition in conditions {
        for conjunct in predicate::conjuncts(condition) {
            if let Access::Columns(a, b) = predicate::access(conjunct) {
                for column in [a, b] {
                    if predicate::split_column(column).0 == own && own.is_some() {
                        columns.push(column);
                    }
                }
            }
        }
    }
    (!columns.is_empty()).then(|| column_list(columns))
}
