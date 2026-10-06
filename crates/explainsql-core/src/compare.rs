//! Before and after: the figures that tell whether a change helped, for two
//! plans of the same statement, such as without and with a suggested index.
//! This compares totals; [`crate::diff`] matches the nodes of two plans.
//!
//! Pages come first. The pages a statement reads, from the cache or from
//! disk, and the pages it spills to temporary files do not depend on what
//! the cache holds, so they compare the same way on every run. Times do, so
//! they decide only when the pages are the same, and only by more than the
//! noise. Measured figures can be the median of several runs.

use serde::Serialize;

use crate::format;
use crate::ir::Plan;
use crate::metrics::{self, Metrics};

/// Changes smaller than this fraction are within the noise.
const NOISE: f64 = 0.1;
/// Time differences under this many milliseconds are within the noise,
/// whatever their fraction.
const MIN_TIME_DIFFERENCE: f64 = 0.1;
/// What PostgreSQL before 18 adds to the cost of a path that an `enable_*`
/// setting disables.
pub const DISABLE_COST: f64 = 1.0e10;

/// What one plan cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Figures {
    /// The planner's estimate for the whole statement, without what
    /// disabled paths add to it.
    pub cost: Option<f64>,
    /// Measured, in milliseconds: the median of the runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_time: Option<f64>,
    /// Pages read, from cache or disk: the median of the runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    /// Pages written to and read back from temporary files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_pages: Option<u64>,
    /// The indexes the plan uses.
    pub indexes: Vec<String>,
    /// The measured runs these figures sum up; 0 for an estimated plan.
    pub runs: usize,
}

/// How the second plan compares with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    /// Fewer pages, or as many and faster; cheaper, when only estimated.
    Better,
    /// More pages, or as many and slower; more expensive, when estimated.
    Worse,
    /// Fewer pages but slower, or the reverse.
    Mixed,
    /// Less than 10% apart.
    Same,
    /// Nothing to compare.
    Unknown,
}

/// The figure a [`Change`] was decided by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    Pages,
    TempPages,
    Time,
    Cost,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub before: Figures,
    pub after: Figures,
    /// Indexes the second plan uses and the first does not.
    pub new_indexes: Vec<String>,
    pub change: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<Basis>,
}

/// The figures of one plan, estimated or measured.
pub fn figures(plan: &Plan, metrics: &Metrics) -> Figures {
    let mut indexes: Vec<String> = Vec::new();
    for node in &plan.nodes {
        if let Some(index) = &node.index_name {
            if !indexes.contains(index) {
                indexes.push(index.clone());
            }
        }
    }
    let buffers = plan.root().buffers;
    Figures {
        cost: planner_cost(plan),
        execution_time: metrics.statement.execution_time,
        pages: buffers.map(|buffers| metrics::blocks(&buffers)),
        temp_pages: buffers.map(|buffers| buffers.temp_read + buffers.temp_written),
        indexes,
        runs: usize::from(plan.root().actuals.is_some()),
    }
}

/// The figures of several measured runs of a statement: the median time
/// and the median page counts. The indexes are those of the first run.
pub fn figures_of_runs(plans: &[Plan]) -> Figures {
    let Some(first) = plans.first() else {
        return Figures {
            cost: None,
            execution_time: None,
            pages: None,
            temp_pages: None,
            indexes: Vec::new(),
            runs: 0,
        };
    };
    let each: Vec<Figures> = plans
        .iter()
        .map(|plan| figures(plan, &metrics::compute(plan)))
        .collect();
    let mut figures = each[0].clone();
    figures.execution_time = median(each.iter().map(|figures| figures.execution_time));
    figures.pages = median_count(each.iter().map(|figures| figures.pages));
    figures.temp_pages = median_count(each.iter().map(|figures| figures.temp_pages));
    figures.runs = if first.root().actuals.is_some() {
        plans.len()
    } else {
        0
    };
    figures
}

/// The planner's cost for the whole statement. Before PostgreSQL 18, a
/// path that an `enable_*` setting disables costs 10¹⁰ more; that is taken
/// out, so that a plan the planner had to use despite a setting compares by
/// what it would really cost.
pub fn planner_cost(plan: &Plan) -> Option<f64> {
    let total = plan.root().estimates?.total_cost;
    let disabled = plan.nodes.iter().any(|node| {
        node.estimates
            .is_some_and(|estimates| estimates.startup_cost >= DISABLE_COST)
    });
    Some(if disabled {
        total - (total / DISABLE_COST).floor() * DISABLE_COST
    } else {
        total
    })
}

pub fn compare(before: &Plan, after: &Plan) -> Comparison {
    between(
        figures(before, &metrics::compute(before)),
        figures(after, &metrics::compute(after)),
    )
}

/// Compares the medians of several measured runs on each side.
pub fn compare_runs(before: &[Plan], after: &[Plan]) -> Comparison {
    between(figures_of_runs(before), figures_of_runs(after))
}

/// Compares two sets of figures.
pub fn between(before: Figures, after: Figures) -> Comparison {
    let new_indexes = after
        .indexes
        .iter()
        .filter(|index| !before.indexes.contains(index))
        .cloned()
        .collect();
    let (change, basis) = judge(&before, &after);
    Comparison {
        before,
        after,
        new_indexes,
        change,
        basis,
    }
}

/// How the measured times compare, beyond the noise.
fn time_change(before: &Figures, after: &Figures) -> Option<Change> {
    before
        .execution_time
        .zip(after.execution_time)
        .map(|(before, after)| {
            if (before - after).abs() < MIN_TIME_DIFFERENCE {
                Change::Same
            } else {
                change(before, after)
            }
        })
}

/// Pages first, then pages spilled to temporary files, then time; the
/// planner's cost when nothing was measured. Pages and time pointing in
/// opposite directions make a mixed result.
fn judge(before: &Figures, after: &Figures) -> (Change, Option<Basis>) {
    let time = time_change(before, after);
    let pages = before.pages.zip(after.pages);
    let io = [
        (Basis::Pages, pages),
        (Basis::TempPages, before.temp_pages.zip(after.temp_pages)),
    ]
    .into_iter()
    .find_map(|(basis, counts)| {
        let (before, after) = counts?;
        // Page counts are far below 2⁵³: the conversion is exact.
        #[allow(clippy::cast_precision_loss)]
        let change = change(before as f64, after as f64);
        (change != Change::Same).then_some((change, basis))
    });
    match (io, time) {
        (Some((Change::Better, basis)), Some(Change::Worse))
        | (Some((Change::Worse, basis)), Some(Change::Better)) => (Change::Mixed, Some(basis)),
        (Some((change, basis)), _) => (change, Some(basis)),
        (None, Some(time)) => (time, Some(Basis::Time)),
        (None, None) if pages.is_some() => (Change::Same, Some(Basis::Pages)),
        (None, None) => match before.cost.zip(after.cost) {
            Some((before, after)) => (change(before, after), Some(Basis::Cost)),
            None => (Change::Unknown, None),
        },
    }
}

/// More than 10% apart, either way, or the same.
fn change(before: f64, after: f64) -> Change {
    if after < before / (1.0 + NOISE) {
        Change::Better
    } else if after > before * (1.0 + NOISE) {
        Change::Worse
    } else {
        Change::Same
    }
}

fn median(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut values: Vec<f64> = values.collect::<Option<_>>()?;
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn median_count(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut values: Vec<u64> = values.collect::<Option<_>>()?;
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

impl Comparison {
    /// One line, pages first: `Pages 21,600 → 43 (502× fewer), execution
    /// 186.1 ms → 0.412 ms (452× faster)`; the estimated cost when nothing
    /// was measured.
    pub fn summary(&self) -> String {
        let mut summary = self.details();
        if let Some(first) = summary.get(..1) {
            summary.replace_range(..1, &first.to_uppercase());
        }
        summary
    }

    /// The summary for the middle of a sentence: `pages 21,600 → 43 (502×
    /// fewer), …`.
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        if let (Some(before), Some(after)) = (self.before.pages, self.after.pages) {
            #[allow(clippy::cast_precision_loss)]
            let ratio = ratio(before as f64, after as f64, "fewer", "more");
            parts.push(format!("pages {} → {}{ratio}", count(before), count(after)));
        }
        if let (Some(before), Some(after)) = (self.before.temp_pages, self.after.temp_pages) {
            if before > 0 || after > 0 {
                parts.push(format!(
                    "temporary files {} → {} pages",
                    count(before),
                    count(after)
                ));
            }
        }
        match (self.before.execution_time, self.after.execution_time) {
            (Some(before), Some(after)) => parts.push(format!(
                "execution {} → {}{}",
                format::duration(before),
                format::duration(after),
                if (before - after).abs() < MIN_TIME_DIFFERENCE {
                    String::new()
                } else {
                    ratio(before, after, "faster", "slower")
                }
            )),
            _ => {
                if let (Some(before), Some(after)) = (self.before.cost, self.after.cost) {
                    parts.push(format!(
                        "estimated cost {before:.0} → {after:.0}{}",
                        ratio(before, after, "cheaper", "more expensive")
                    ));
                }
            }
        }
        let runs = self.before.runs.max(self.after.runs);
        if runs > 1 {
            parts.push(format!("median of {runs} runs"));
        }
        parts.join(", ")
    }

    /// How the times alone compare, when both plans were measured: changes
    /// under 10% or 0.1 ms count as the same.
    pub fn time_change(&self) -> Option<Change> {
        time_change(&self.before, &self.after)
    }

    /// Whether the second plan is better: by pages, then by time when
    /// measured; by the planner's estimate otherwise. Changes under 10%
    /// do not count.
    pub fn improved(&self) -> bool {
        self.change == Change::Better
    }

    /// The change in words: `better`, `no different, within 10%`.
    pub fn describe(&self) -> &'static str {
        match (self.change, self.basis) {
            (Change::Better, Some(Basis::Cost)) => "cheaper by the planner's estimate",
            (Change::Better, _) => "better",
            (Change::Worse, Some(Basis::Cost)) => "more expensive by the planner's estimate",
            (Change::Worse, _) => "worse",
            (Change::Mixed, _) => "mixed: fewer pages but slower, or the reverse",
            (Change::Same, _) => "no different, within 10%",
            (Change::Unknown, _) => "not comparable",
        }
    }
}

fn count(value: u64) -> String {
    format::grouped(i64::try_from(value).unwrap_or(i64::MAX))
}

/// ` (12× faster)`, or nothing for a change under 10%.
fn ratio(before: f64, after: f64, better: &str, worse: &str) -> String {
    if before <= 0.0 || after <= 0.0 {
        return String::new();
    }
    if after < before / (1.0 + NOISE) {
        format!(" ({} {better})", format::factor(before / after))
    } else if after > before * (1.0 + NOISE) {
        format!(" ({} {worse})", format::factor(after / before))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEQ_SCAN: &str = "\
Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.900 ms";

    const INDEX_SCAN: &str = "\
Index Scan using orders_customer_id_idx on orders  (cost=0.42..12.60 rows=10 width=64) (actual time=0.020..0.031 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=13
Execution Time: 0.050 ms";

    fn plan(text: &str) -> Plan {
        crate::parse(text).unwrap()
    }

    /// The seq scan's plan with another execution time and page count.
    fn seq_scan(ms: &str, hit: u64) -> Plan {
        plan(
            &SEQ_SCAN
                .replace("11.900 ms", &format!("{ms} ms"))
                .replace("hit=2031 read=386", &format!("hit={hit}")),
        )
    }

    #[test]
    fn compares_two_plans() {
        let comparison = compare(&plan(SEQ_SCAN), &plan(INDEX_SCAN));
        assert_eq!(comparison.new_indexes, ["orders_customer_id_idx"]);
        assert!(comparison.improved());
        assert_eq!(comparison.basis, Some(Basis::Pages));
        assert_eq!(
            comparison.summary(),
            "Pages 2,417 → 13 (186× fewer), execution 11.9 ms → 0.050 ms (238× faster)"
        );
        // And the other way round.
        let comparison = compare(&plan(INDEX_SCAN), &plan(SEQ_SCAN));
        assert_eq!(comparison.change, Change::Worse);
    }

    #[test]
    fn pages_decide_before_time() {
        // Fewer pages and no faster: better, whatever the cache did.
        let comparison = compare(&seq_scan("11.900", 2417), &seq_scan("11.950", 1200));
        assert_eq!(
            (comparison.change, comparison.basis),
            (Change::Better, Some(Basis::Pages))
        );
        // As many pages and much faster: faster only because the cache was
        // warm still counts as better, as the time is all that differs...
        let comparison = compare(&seq_scan("11.900", 2417), &seq_scan("5.000", 2417));
        assert_eq!(
            (comparison.change, comparison.basis),
            (Change::Better, Some(Basis::Time))
        );
        // ... but fewer pages and much slower is mixed.
        let comparison = compare(&seq_scan("11.900", 2417), &seq_scan("40.000", 100));
        assert_eq!(comparison.change, Change::Mixed);
        assert!(!comparison.improved());
    }

    #[test]
    fn small_differences_are_noise() {
        // 5% fewer pages and 5% faster: no change.
        let comparison = compare(&seq_scan("11.900", 2417), &seq_scan("11.300", 2300));
        assert_eq!(comparison.change, Change::Same);
        assert!(!comparison.improved());
        // 30% faster, but by less than 0.1 ms.
        let comparison = compare(&seq_scan("0.200", 20), &seq_scan("0.140", 20));
        assert_eq!(comparison.change, Change::Same);
        assert_eq!(
            comparison.summary(),
            "Pages 20 → 20, execution 0.200 ms → 0.140 ms"
        );
    }

    #[test]
    fn estimated_plans_compare_by_cost() {
        let before = plan("Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64)");
        let after = plan(
            "Index Scan using orders_customer_id_idx on orders  (cost=0.42..46.00 rows=10 width=64)\n  Index Cond: (customer_id = 4242)",
        );
        let comparison = compare(&before, &after);
        assert_eq!(
            (comparison.change, comparison.basis),
            (Change::Better, Some(Basis::Cost))
        );
        assert_eq!(comparison.before.runs, 0);
        assert_eq!(
            comparison.summary(),
            "Estimated cost 4917 → 46 (107× cheaper)"
        );
        // A plan forced with enable_seqscan = off before PostgreSQL 18
        // compares by what it would really cost.
        let forced =
            plan("Seq Scan on orders  (cost=10000000000.00..10000004917.00 rows=10 width=64)");
        assert_eq!(planner_cost(&forced), Some(4917.0));
        assert_eq!(compare(&before, &forced).change, Change::Same);
    }

    #[test]
    fn spills_count_like_pages() {
        let spill = |temp: u64, ms: &str| {
            plan(&format!(
                "Sort  (cost=1.00..2.00 rows=1000 width=4) (actual time=10.000..20.000 rows=1000 loops=1)\n  Sort Key: a\n  Buffers: shared hit=100{}\n  ->  Seq Scan on t  (cost=0.00..1.00 rows=1000 width=4) (actual time=0.010..5.000 rows=1000 loops=1)\n        Buffers: shared hit=100\nExecution Time: {ms} ms",
                if temp > 0 {
                    format!(", temp read={temp} written={temp}")
                } else {
                    String::new()
                }
            ))
        };
        let comparison = compare(&spill(500, "20.500"), &spill(0, "19.000"));
        assert_eq!(
            (comparison.change, comparison.basis),
            (Change::Better, Some(Basis::TempPages))
        );
        assert_eq!(
            comparison.summary(),
            "Pages 100 → 100, temporary files 1,000 → 0 pages, execution 20.5 ms → 19.0 ms"
        );
    }

    #[test]
    fn several_runs_compare_by_their_medians() {
        let before = [
            seq_scan("12.000", 2417),
            seq_scan("30.000", 2417),
            seq_scan("11.000", 2417),
        ];
        let after = [
            seq_scan("6.000", 2417),
            seq_scan("5.000", 2417),
            seq_scan("40.000", 2417),
        ];
        let comparison = compare_runs(&before, &after);
        assert_eq!(comparison.before.execution_time, Some(12.0));
        assert_eq!(comparison.after.execution_time, Some(6.0));
        assert_eq!(comparison.change, Change::Better);
        assert_eq!(
            comparison.summary(),
            "Pages 2,417 → 2,417, execution 12.0 ms → 6.00 ms (2.0× faster), median of 3 runs"
        );
        assert_eq!(
            compare_runs(&[], &[]).change,
            Change::Unknown,
            "nothing to compare"
        );
    }
}
