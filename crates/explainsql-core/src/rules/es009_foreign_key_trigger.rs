//! ES009: a foreign-key trigger on the referenced side that takes a large
//! share of the execution time. Deleting or updating a referenced row makes
//! PostgreSQL look up the rows that reference it; without an index on the
//! referencing columns, every lookup scans the referencing table.
//!
//! Only action triggers count (`RI_ConstraintTrigger_a_…`, or a constraint
//! trigger of a `DELETE` when the plan does not name it). The check triggers
//! of an `INSERT` look up the referenced table's unique index, which always
//! exists.

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;
use crate::ir::Trigger;

pub const RULE: Rule = Rule {
    id: "ES009",
    name: "Slow foreign-key trigger",
};

/// Share of the execution time spent in the trigger.
const MIN_SHARE: f64 = 0.2;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    let Some(total) = context.metrics.statement.total_time else {
        return Vec::new();
    };
    let deleting = context.plan.root().operation.as_deref() == Some("Delete");
    context
        .plan
        .summary
        .triggers
        .iter()
        .filter(|trigger| is_foreign_key_action(trigger, deleting))
        .filter_map(|trigger| {
            let time = trigger.time?;
            let share = time / total;
            (share >= MIN_SHARE).then(|| finding(trigger, time, share))
        })
        .collect()
}

fn is_foreign_key_action(trigger: &Trigger, deleting: bool) -> bool {
    match trigger.name.as_deref() {
        Some(name) if name.starts_with("RI_ConstraintTrigger_") => {
            name.starts_with("RI_ConstraintTrigger_a_")
        }
        Some(_) => false,
        // Without VERBOSE, the text format names only the constraint.
        None => trigger.constraint.is_some() && deleting,
    }
}

fn finding(trigger: &Trigger, time: f64, share: f64) -> Finding {
    let constraint = trigger.constraint.as_deref().unwrap_or("the foreign key");
    let mut facts = Vec::new();
    if let Some(name) = &trigger.name {
        facts.push(evidence("Trigger", name.clone()));
    }
    facts.push(evidence("Constraint", constraint));
    facts.push(evidence("Calls", format::rows(trigger.calls)));
    facts.push(evidence(
        "Time",
        format!(
            "{} ({} of the execution)",
            format::duration(time),
            format::percent(share)
        ),
    ));
    if trigger.calls > 0.0 {
        facts.push(evidence(
            "Time per call",
            format::duration(time / trigger.calls),
        ));
    }
    Finding {
        rule: RULE,
        severity: Severity::from_share(Some(share)),
        node: None,
        summary: format!(
            "The foreign-key trigger for {constraint} took {} of the execution time",
            format::percent(share)
        ),
        evidence: facts,
        action: format!(
            "Index the referencing columns of {constraint} on the referencing table: each deleted or updated row looks up the rows that refer to it, and without an index every lookup scans that table."
        ),
    }
}
