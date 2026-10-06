//! Everything derived from a plan, and the one sentence that sums it up.

use serde::Serialize;

use crate::advisor::{self, Advice};
use crate::counterfactual::Answer;
use crate::format;
use crate::ir::Plan;
use crate::locks::Footprint;
use crate::metrics::{self, Metrics};
use crate::params::Sensitivity;
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
    /// What the database said when asked why the planner chose its plan
    /// (connected mode); empty until asked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub counterfactuals: Vec<Answer>,
    /// How the plan depends on the statement's parameters (connected mode,
    /// `--params`), when the plan is the generic plan of a statement with
    /// parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Sensitivity>,
    /// The locks the statement takes and what they mean (connected mode,
    /// `--locks` and `L` in the viewer): one footprint, or with `--params`
    /// those of an execution of the generic plan and of a custom plan.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<Footprint>,
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
        counterfactuals: Vec::new(),
        parameters: None,
        locks: Vec::new(),
    }
}

impl Analysis {
    /// Records answers from the database, replacing earlier answers about
    /// the same nodes and questions, and puts them into the advice.
    pub fn record(&mut self, answers: Vec<Answer>) {
        for answer in answers {
            self.counterfactuals
                .retain(|other| !(other.node == answer.node && other.question == answer.question));
            self.counterfactuals.push(answer);
        }
        crate::counterfactual::annotate(&mut self.advice, &self.counterfactuals);
    }
}

/// From this share of the time spent reading pages, the cache was cold.
const COLD_CACHE: f64 = 0.5;

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
    // Waiting for reads most of the time: the cache was cold.
    if let Some(share) = statement
        .io
        .filter(|io| io.total() > 0.0)
        .and_then(|io| Some(io.share? * io.read / io.total()))
        .filter(|&share| share >= COLD_CACHE)
    {
        sentence.push_str(&format!(
            " {} of the time went to reading pages that were not in shared buffers: the cache was cold, and a second run may be faster.",
            format::percent(share)
        ));
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

        // Waiting for reads most of the time.
        let cold = crate::parse(
            "\
Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..100.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared read=2417
  I/O Timings: shared read=80.000
Execution Time: 100.900 ms",
        )
        .unwrap();
        assert_eq!(
            analyze(&cold).verdict,
            "100.9 ms. 100% of it in Seq Scan on orders, which reads 200,000 rows to keep 10. 79% of the time went to reading pages that were not in shared buffers: the cache was cold, and a second run may be faster."
        );

        let estimated =
            crate::parse("Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64)").unwrap();
        assert_eq!(
            analyze(&estimated).verdict,
            "Estimated plan only: run EXPLAIN ANALYZE for actual times and row counts. Estimated total cost: 4,917."
        );
    }
}
