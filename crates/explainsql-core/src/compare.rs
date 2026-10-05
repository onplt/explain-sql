//! Before and after: the figures that tell whether a change helped, for two
//! plans of the same statement, such as without and with a suggested index.
//! This compares totals, not trees; matching nodes between plans comes
//! later.

use serde::Serialize;

use crate::format;
use crate::ir::Plan;
use crate::metrics::{self, Metrics};

/// What one plan cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Figures {
    /// The planner's estimate for the whole statement.
    pub cost: Option<f64>,
    /// Measured, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time: Option<f64>,
    /// Pages read, from cache or disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    /// The indexes the plan uses.
    pub indexes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub before: Figures,
    pub after: Figures,
    /// Indexes the second plan uses and the first does not.
    pub new_indexes: Vec<String>,
}

pub fn figures(plan: &Plan, metrics: &Metrics) -> Figures {
    let mut indexes: Vec<String> = Vec::new();
    for node in &plan.nodes {
        if let Some(index) = &node.index_name {
            if !indexes.contains(index) {
                indexes.push(index.clone());
            }
        }
    }
    Figures {
        cost: plan.root().estimates.map(|estimates| estimates.total_cost),
        execution_time: metrics.statement.execution_time,
        pages: plan.root().buffers.map(|buffers| metrics::blocks(&buffers)),
        indexes,
    }
}

pub fn compare(before: &Plan, after: &Plan) -> Comparison {
    let before = figures(before, &metrics::compute(before));
    let after = figures(after, &metrics::compute(after));
    let new_indexes = after
        .indexes
        .iter()
        .filter(|index| !before.indexes.contains(index))
        .cloned()
        .collect();
    Comparison {
        before,
        after,
        new_indexes,
    }
}

impl Comparison {
    /// One line: `Execution 186.1 ms → 0.412 ms (452× faster), pages 21,600
    /// → 43`, measured if possible, else the estimated cost.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match (self.before.execution_time, self.after.execution_time) {
            (Some(before), Some(after)) => parts.push(format!(
                "Execution {} → {}{}",
                format::duration(before),
                format::duration(after),
                ratio(before, after, "faster", "slower")
            )),
            _ => {
                if let (Some(before), Some(after)) = (self.before.cost, self.after.cost) {
                    parts.push(format!(
                        "Estimated cost {before:.0} → {after:.0}{}",
                        ratio(before, after, "cheaper", "more expensive")
                    ));
                }
            }
        }
        if let (Some(before), Some(after)) = (self.before.pages, self.after.pages) {
            parts.push(format!(
                "pages {} → {}",
                format::grouped(i64::try_from(before).unwrap_or(i64::MAX)),
                format::grouped(i64::try_from(after).unwrap_or(i64::MAX))
            ));
        }
        parts.join(", ")
    }

    /// Whether the second plan is better: faster when measured, else
    /// cheaper by the planner's estimate.
    pub fn improved(&self) -> bool {
        match (self.before.execution_time, self.after.execution_time) {
            (Some(before), Some(after)) => after < before,
            _ => matches!((self.before.cost, self.after.cost), (Some(b), Some(a)) if a < b),
        }
    }
}

/// ` (12× faster)`, or nothing for a change under 10%.
fn ratio(before: f64, after: f64, better: &str, worse: &str) -> String {
    if before <= 0.0 || after <= 0.0 {
        return String::new();
    }
    if after < before / 1.1 {
        format!(" ({} {better})", format::factor(before / after))
    } else if after > before * 1.1 {
        format!(" ({} {worse})", format::factor(after / before))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_two_plans() {
        let before = crate::parse(
            "\
Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.900 ms",
        )
        .unwrap();
        let after = crate::parse(
            "\
Index Scan using orders_customer_id_idx on orders  (cost=0.42..12.60 rows=10 width=64) (actual time=0.020..0.031 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=13
Execution Time: 0.050 ms",
        )
        .unwrap();
        let comparison = compare(&before, &after);
        assert_eq!(comparison.new_indexes, ["orders_customer_id_idx"]);
        assert!(comparison.improved());
        assert_eq!(
            comparison.summary(),
            "Execution 11.9 ms → 0.050 ms (238× faster), pages 2,417 → 13"
        );
    }
}
