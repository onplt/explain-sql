//! ES010: a nested loop that pairs every outer row with every inner row,
//! with no condition between the two sides.
//!
//! Silent when either side returns a single row (a deliberate cross join with
//! a one-row subquery), when the product is small, when the inner side refers
//! to the outer one (a parameterized scan), and for semi and anti joins.

use serde_json::Value;

use super::{Context, Finding, Rule, Severity, evidence, qualifier};
use crate::format;
use crate::ir::{Node, PredicateKind, Relationship};

pub const RULE: Rule = Rule {
    id: "ES010",
    name: "Cartesian product",
};

/// Output rows from which a product is worth reporting.
const MIN_ROWS: f64 = 1000.0;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    context
        .nodes()
        .filter(|node| node.node_type == "Nested Loop")
        .filter(|node| {
            matches!(
                node.join_type.as_deref(),
                None | Some("Inner" | "Left" | "Full" | "Right")
            )
        })
        .filter(|node| node.predicate(PredicateKind::JoinFilter).is_none())
        .filter_map(|node| product(context, node))
        .collect()
}

fn product(context: &Context, join: &Node) -> Option<Finding> {
    let (outer, inner) = (
        context.child(join, Relationship::Outer)?,
        context.child(join, Relationship::Inner)?,
    );
    let (join_actuals, outer_actuals, inner_actuals) =
        (join.actuals?, outer.actuals?, inner.actuals?);
    let outer_rows = outer_actuals.rows * outer_actuals.loops as f64;
    let inner_rows = inner_actuals.rows;
    let output = join_actuals.rows * join_actuals.loops as f64;
    if outer_rows < 2.0
        || inner_rows < 2.0
        || output < MIN_ROWS
        || output < 0.9 * outer_rows * inner_rows
    {
        return None;
    }
    if refers_to_outer(context, outer, inner) {
        return None;
    }
    let sides = |node: &Node| relations(context, node).join(", ");
    Some(Finding {
        rule: RULE,
        severity: Severity::from_share(context.inclusive_share(join)).max(Severity::Medium),
        node: Some(join.id),
        summary: format!(
            "{} pairs each of {} outer rows with all {} inner rows, with no condition between them",
            format::node(join),
            format::rows(outer_rows),
            format::rows(inner_rows)
        ),
        evidence: vec![
            evidence("Outer rows", format::rows(outer_rows)),
            evidence("Inner rows per loop", format::rows(inner_rows)),
            evidence("Rows produced", format::rows(output)),
        ],
        action: format!(
            "Check the query for a missing join condition between {} and {}.",
            sides(outer),
            sides(inner)
        ),
    })
}

/// The relations scanned below a node.
fn relations(context: &Context, node: &Node) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if let Some(name) = current.relation_name.as_ref().or(current.alias.as_ref()) {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        stack.extend(context.plan.children(current.id));
    }
    names
}

/// Whether a condition on the inner side mentions a relation of the outer
/// side: then the inner side is evaluated per outer row and the join is not
/// a product.
fn refers_to_outer(context: &Context, outer: &Node, inner: &Node) -> bool {
    let mut aliases = Vec::new();
    let mut stack = vec![outer];
    while let Some(node) = stack.pop() {
        if let Some(alias) = qualifier(node) {
            aliases.push(format!("{alias}."));
        }
        stack.extend(context.plan.children(node.id));
    }
    let mut stack = vec![inner];
    while let Some(node) = stack.pop() {
        let texts = node
            .predicates
            .iter()
            .map(|p| p.text.as_str())
            .chain(node.extra.values().filter_map(Value::as_str));
        for text in texts {
            if aliases.iter().any(|alias| text.contains(alias.as_str())) {
                return true;
            }
        }
        stack.extend(context.plan.children(node.id));
    }
    false
}
