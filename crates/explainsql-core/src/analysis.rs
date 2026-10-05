//! Everything derived from a plan, and the one sentence that sums it up.

use serde::Serialize;

use crate::advisor::{self, Advice};
use crate::format;
use crate::ir::Plan;
use crate::metrics::{self, Metrics};
use crate::rules::{self, Finding};

/// The metrics, the findings and the verdict for a plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Analysis {
    /// Where the time went, in one sentence: `11.9 ms. 100% of it in Seq Scan
    /// on orders, which reads 200,000 rows to keep 10.`
    pub verdict: String,
    pub metrics: Metrics,
    /// Most severe first.
    pub findings: Vec<Finding>,
    /// Index candidates, rewrites, and why slow scans get no index.
    pub advice: Vec<Advice>,
}

/// Computes the metrics and runs the rules.
pub fn analyze(plan: &Plan) -> Analysis {
    let metrics = metrics::compute(plan);
    let findings = rules::check(plan, &metrics);
    let verdict = verdict(plan, &metrics, &findings);
    let advice = advisor::advise(plan, &metrics, &findings);
    Analysis {
        verdict,
        metrics,
        findings,
        advice,
    }
}

fn verdict(plan: &Plan, metrics: &Metrics, findings: &[Finding]) -> String {
    let statement = &metrics.statement;
    let Some(total) = statement.total_time else {
        let cost = plan
            .root()
            .estimates
            .map(|estimates| {
                format!(
                    " Estimated total cost: {}.",
                    format::rows(estimates.total_cost.round())
                )
            })
            .unwrap_or_default();
        return format!(
            "Estimated plan only: run EXPLAIN ANALYZE for actual times and row counts.{cost}"
        );
    };
    let mut sentence = format!("{}.", format::duration(total));
    let mut mentioned = None;
    let slowest_trigger = plan
        .summary
        .triggers
        .iter()
        .filter_map(|trigger| Some((trigger, trigger.time?)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    match (slowest_trigger, statement.hotspots.first()) {
        (Some((trigger, time)), _) if time / total >= 0.5 => {
            let name = trigger
                .constraint
                .as_deref()
                .map(|constraint| format!("the trigger for {constraint}"))
                .or_else(|| {
                    trigger
                        .name
                        .as_deref()
                        .map(|name| format!("trigger {name}"))
                })
                .unwrap_or_else(|| "a trigger".to_owned());
            sentence.push_str(&format!(
                " {} of it in {name}.",
                format::percent(time / total)
            ));
        }
        (_, Some(&hottest)) if statement.tree_time.is_some() => {
            let node = plan.node(hottest);
            let label = format::node(node);
            let share = metrics.node(hottest).time_share.unwrap_or(0.0);
            sentence.push_str(&format!(" {} of it in {label}", format::percent(share)));
            match findings
                .iter()
                .find(|finding| finding.node == Some(hottest))
            {
                Some(finding) => {
                    mentioned = Some(finding);
                    match finding.summary.strip_prefix(&label) {
                        Some(rest) => sentence.push_str(&format!(", which{rest}.")),
                        None => sentence.push_str(&format!(". {}.", finding.summary)),
                    }
                }
                None => sentence.push('.'),
            }
        }
        _ => sentence.push_str(" The plan has no per-node times (TIMING OFF)."),
    }
    // The most severe finding, if the sentence does not cover it yet.
    if let Some(finding) = findings.first() {
        let covered = mentioned.is_some_and(|mentioned| std::ptr::eq(mentioned, finding))
            || (finding.node.is_none()
                && finding.rule.id == "ES009"
                && sentence.contains("trigger"));
        if !covered {
            sentence.push_str(&format!(" {}.", finding.summary));
        }
    }
    sentence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_up_where_the_time_went() {
        let plan = crate::parse(
            "\
Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.899 ms",
        )
        .unwrap();
        assert_eq!(
            analyze(&plan).verdict,
            "11.9 ms. 100% of it in Seq Scan on orders, which reads 200,000 rows to keep 10."
        );

        let estimated =
            crate::parse("Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64)").unwrap();
        assert_eq!(
            analyze(&estimated).verdict,
            "Estimated plan only: run EXPLAIN ANALYZE for actual times and row counts. Estimated total cost: 4,917."
        );
    }
}
