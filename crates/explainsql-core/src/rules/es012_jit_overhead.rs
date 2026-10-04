//! ES012: JIT compilation takes most of the execution time. The planner
//! turns JIT on by estimated cost, so a query that is estimated as expensive
//! but runs quickly can spend more time compiling than executing.

use super::{Context, Finding, Rule, Severity, evidence};
use crate::format;

pub const RULE: Rule = Rule {
    id: "ES012",
    name: "JIT overhead dominates",
};

/// Share of the execution time spent compiling.
const MIN_SHARE: f64 = 0.5;

pub(super) fn check(context: &Context) -> Vec<Finding> {
    let statement = &context.metrics.statement;
    let (Some(jit), Some(total)) = (context.plan.summary.jit.as_ref(), statement.execution_time)
    else {
        return Vec::new();
    };
    let Some(timing) = jit.timing else {
        return Vec::new();
    };
    let share = timing.total / total;
    if share < MIN_SHARE {
        return Vec::new();
    }
    let mut action = "Raise jit_above_cost so that JIT only starts for queries that run long enough to benefit, or set jit = off for OLTP workloads.".to_owned();
    if timing.inlining + timing.optimization > timing.total / 2.0 {
        action.push_str(" Most of the time went to inlining and optimization: raising jit_inline_above_cost and jit_optimize_above_cost keeps JIT but drops its most expensive steps.");
    }
    vec![Finding {
        rule: RULE,
        severity: Severity::from_share(Some(share)),
        node: None,
        summary: format!(
            "JIT compilation took {} of the {} execution ({})",
            format::duration(timing.total),
            format::duration(total),
            format::percent(share)
        ),
        evidence: vec![
            evidence("Functions compiled", jit.functions.to_string()),
            evidence(
                "Compilation",
                format!(
                    "generation {}, inlining {}, optimization {}, emission {}",
                    format::duration(timing.generation),
                    format::duration(timing.inlining),
                    format::duration(timing.optimization),
                    format::duration(timing.emission)
                ),
            ),
            evidence("Execution time", format::duration(total)),
        ],
        action,
    }]
}
