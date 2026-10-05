//! ES002: a node returned 10× more or fewer rows per loop than the planner
//! estimated, and the error starts there rather than being passed up from a
//! child.
//!
//! Estimates drive the choice of join methods, so misestimates matter most on
//! the inputs of joins. Silent when both counts are small, when the node
//! returned fewer rows than estimated but a node above may have stopped it
//! early, for nodes that pass their input's rows through (a Sort, a Hash, a
//! Materialize) or report none (bitmap nodes), and for the Recursive Union of
//! a recursive CTE, whose depth the planner cannot know. A CTE Scan inherits
//! the error of the CTE it reads.

use super::{Context, Finding, Rule, Severity, column_list, evidence, relation};
use crate::expr;
use crate::format;
use crate::ir::{Node, PredicateKind, Relationship};

pub const RULE: Rule = Rule {
    id: "ES002",
    name: "Row misestimate",
};

/// The smallest error worth reporting.
const MIN_FACTOR: f64 = 10.0;
/// Rows per loop (the larger of actual and estimated) below which an error
/// does not matter, unless the loops add up to `MIN_TOTAL_ROWS`.
const MIN_ROWS: f64 = 100.0;
const MIN_TOTAL_ROWS: f64 = 10_000.0;

/// Nodes whose row count follows their input's, that report no rows (bitmap
/// nodes pass a bitmap), or whose size the planner cannot know.
const SKIPPED: [&str; 12] = [
    "Hash",
    "Sort",
    "Incremental Sort",
    "Materialize",
    "Memoize",
    "Gather",
    "Gather Merge",
    "LockRows",
    "Bitmap Index Scan",
    "BitmapAnd",
    "BitmapOr",
    "Recursive Union",
];

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| {
            !SKIPPED.contains(&node.node_type.as_str()) && node.node_type != "ModifyTable"
        })
        .filter_map(|node| misestimate(context, node))
        .collect()
}

fn misestimate(context: &Context, node: &Node) -> Option<Finding> {
    let metrics = context.metrics(node);
    let error = metrics.misestimate?;
    let (estimates, actuals) = (node.estimates?, node.actuals?);
    if error.factor < MIN_FACTOR || (!error.underestimated && metrics.may_stop_early) {
        return None;
    }
    let larger = estimates.rows.max(actuals.rows);
    if larger < MIN_ROWS && larger * (actuals.loops as f64) < MIN_TOTAL_ROWS {
        return None;
    }
    // Only where the error starts: a node that merely passes on a child's
    // error adds less than MIN_FACTOR of its own.
    let inherited = context
        .plan
        .children(node.id)
        .filter(|child| {
            !matches!(
                child.relationship,
                Some(Relationship::InitPlan | Relationship::SubPlan)
            )
        })
        .chain(cte_read_by(context, node))
        .filter_map(|child| context.metrics(child).misestimate)
        .filter(|child| child.underestimated == error.underestimated)
        .map(|child| child.factor)
        .fold(1.0, f64::max);
    if error.factor / inherited < MIN_FACTOR {
        return None;
    }

    let join = consumer_join(context, node);
    let direction = if error.underestimated {
        "more"
    } else {
        "fewer"
    };
    let summary = format!(
        "{} returned {} rows {}where the planner expected {}: {} {direction}",
        format::node(node),
        format::rows(actuals.rows),
        if actuals.loops > 1 { "per loop " } else { "" },
        format::rows(estimates.rows),
        format::factor(error.factor),
    );
    let mut facts = vec![
        evidence("Estimated rows", format::rows(estimates.rows)),
        evidence("Actual rows", format::rows(actuals.rows)),
    ];
    if actuals.loops > 1 {
        facts.push(evidence("Loops", format::rows(actuals.loops as f64)));
    }
    if let Some(join) = join {
        facts.push(evidence("Input of", format::node(join)));
    }

    let severity = match join {
        Some(join) => Severity::from_share(context.inclusive_share(join)).max(Severity::Medium),
        None => Severity::Low,
    };
    Some(Finding {
        rule: RULE,
        severity,
        node: Some(node.id),
        summary,
        evidence: facts,
        action: action(node, join.is_some()),
    })
}

/// The subtree of the CTE that a CTE Scan reads.
fn cte_read_by<'a>(context: &Context<'a>, node: &Node) -> Option<&'a Node> {
    let name = node
        .cte_name
        .as_deref()
        .filter(|_| node.node_type == "CTE Scan")?;
    context.nodes().find(|candidate| {
        candidate.relationship == Some(Relationship::InitPlan)
            && candidate
                .subplan_name
                .as_deref()
                .and_then(|subplan| subplan.strip_prefix("CTE "))
                == Some(name)
    })
}

/// The join that reads the node's rows, if any, looking through a Hash.
fn consumer_join<'a>(context: &Context<'a>, node: &Node) -> Option<&'a Node> {
    let mut parent = context.parent(node)?;
    if matches!(
        parent.node_type.as_str(),
        "Hash" | "Materialize" | "Memoize" | "Sort"
    ) {
        parent = context.parent(parent)?;
    }
    matches!(
        parent.node_type.as_str(),
        "Nested Loop" | "Hash Join" | "Merge Join"
    )
    .then_some(parent)
}

fn action(node: &Node, feeds_join: bool) -> String {
    let consequence = if feeds_join {
        " The join above chose its method for the estimated row count."
    } else {
        ""
    };
    if node.relation_name.is_none() {
        return format!(
            "The planner misjudged how many rows this step produces, usually because the statistics of the columns it combines are stale or the columns are correlated. ANALYZE the tables involved; extended statistics (CREATE STATISTICS) on the join or grouping columns can help.{consequence}"
        );
    }
    let table = relation(node);
    let accesses: Vec<expr::Access> = node
        .predicates
        .iter()
        .filter(|p| {
            matches!(
                p.kind,
                PredicateKind::Filter | PredicateKind::IndexCond | PredicateKind::RecheckCond
            )
        })
        .flat_map(|p| expr::conjuncts(&p.text))
        .map(expr::access)
        .collect();
    // Conditions the planner cannot estimate from column statistics.
    if let Some(column) = accesses.iter().find_map(|access| match access {
        expr::Access::Wrapped { column, .. } => Some(*column),
        _ => None,
    }) {
        let (_, name) = expr::split_column(column);
        return format!(
            "The planner has no statistics for an expression of {name}, so it guessed. Rewriting the condition so that {name} stands alone fixes the estimate; otherwise CREATE STATISTICS on the expression (PostgreSQL 14 and later) gives the planner something to go on.{consequence}"
        );
    }
    if let Some(value) = accesses.iter().find_map(|access| match access {
        expr::Access::Column { value, .. } if is_runtime_value(value) => Some(*value),
        _ => None,
    }) {
        return format!(
            "The condition compares with {value}, a value known only when the query runs, so the planner used a default guess. ANALYZE cannot change that; it matters only if a plan choice above depends on this estimate.{consequence}"
        );
    }
    let columns: Vec<&str> = accesses
        .iter()
        .filter_map(|access| match access {
            expr::Access::Column { column, .. } => Some(*column),
            _ => None,
        })
        .collect();
    let mut action = format!("Run ANALYZE {table}: its statistics may be out of date.");
    match columns.len() {
        0 => {}
        1 => action.push_str(&format!(
            " If the estimate stays off, raise the statistics target of {} (ALTER TABLE {table} ALTER COLUMN … SET STATISTICS).",
            column_list(columns)
        )),
        _ => action.push_str(&format!(
            " If it stays off, the columns in the condition are probably correlated: CREATE STATISTICS ON {} FROM {table}.",
            column_list(columns)
        )),
    }
    action.push_str(consequence);
    action
}

/// A parameter or the result of a subquery: `$0`, `(InitPlan 1).col1`.
fn is_runtime_value(value: &str) -> bool {
    value.starts_with('$') || value.contains("InitPlan ") || value.contains("SubPlan ")
}
