//! A gate for continuous integration: whether a plan passes, from its
//! findings and from how it compares with the plan it is locked to.
//!
//! A plan fails when a finding is at least as severe as asked
//! (`--fail-on`), or when it is worse than its locked plan by pages, or by
//! the planner's estimated cost when neither plan was run. Time alone does
//! not fail a plan: on a shared CI runner it changes from one run to the
//! next for the same pages, so a slower run with the same pages is a note.
//! With `strict`, any change of the plan's shape fails it too, as a
//! contract: the change must be locked on purpose.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::analysis::Analysis;
use crate::compare::{Basis, Change};
use crate::diff::{self, PlanDiff};
use crate::fingerprint;
use crate::ir::Plan;
use crate::rules::Severity;

/// When a plan fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// Fail on findings at least this severe.
    pub fail_on: Option<Severity>,
    /// Fail when the plan's shape changed, even if it is not worse.
    pub strict: bool,
}

/// How a plan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// A finding or the comparison with the locked plan fails it.
    Failed,
    /// Not locked yet, and no finding fails it.
    New,
    Passed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Failed => "FAIL",
            Status::New => "NEW",
            Status::Passed => "PASS",
        }
    }
}

/// One plan, checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Checked {
    /// The statement or plan file, as the report names it.
    pub name: String,
    pub status: Status,
    /// The plan's [shape id](fingerprint::id).
    pub shape: String,
    /// Why the plan failed, one sentence each.
    pub reasons: Vec<String>,
    /// What is worth knowing but does not fail the plan.
    pub notes: Vec<String>,
    /// The comparison with the locked plan, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<PlanDiff>,
    #[serde(skip)]
    pub plan: Plan,
    #[serde(skip)]
    pub analysis: Analysis,
    /// The locked plan, when there is one.
    #[serde(skip)]
    pub baseline: Option<Plan>,
    /// The findings that fail the plan, as indexes into its analysis.
    #[serde(skip)]
    pub failing: Vec<usize>,
}

/// Checks a plan against its findings and its locked plan.
pub fn check(
    name: &str,
    plan: Plan,
    analysis: Analysis,
    baseline: Option<Plan>,
    policy: Policy,
) -> Checked {
    let mut reasons = Vec::new();
    let mut notes = Vec::new();
    let mut failing = Vec::new();
    if let Some(threshold) = policy.fail_on {
        for (index, finding) in analysis.findings.iter().enumerate() {
            if finding.severity >= threshold {
                failing.push(index);
                reasons.push(format!(
                    "{} {}: {}.",
                    finding.rule.id, finding.rule.name, finding.summary
                ));
            }
        }
    }
    let diff = baseline
        .as_ref()
        .map(|baseline| diff::diff(baseline, &plan));
    if let Some(diff) = &diff {
        let worse = diff.comparison.change == Change::Worse;
        let decided_by = diff.comparison.basis;
        let regression = worse
            && matches!(
                decided_by,
                Some(Basis::Pages | Basis::TempPages | Basis::Cost)
            );
        let details = diff.comparison.details();
        // The plan's main change, as a sentence of its own.
        let main = diff
            .changes
            .iter()
            .find(|change| change.kind.structural())
            .map(|change| format!(" {}.", change.summary))
            .unwrap_or_default();
        if regression {
            let by = if decided_by == Some(Basis::Cost) {
                " by the planner's estimate"
            } else {
                ""
            };
            reasons.push(format!("Worse than the locked plan{by}: {details}.{main}"));
        } else if !diff.shapes.same() && policy.strict {
            reasons.push(format!(
                "The plan changed from the locked one ({details}), and --strict asks for the same plan.{main}"
            ));
        } else if !diff.shapes.same() {
            notes.push(format!(
                "The plan changed, {}: {details}.{main}",
                diff.comparison.describe()
            ));
        } else if worse || diff.comparison.change == Change::Mixed {
            notes.push(format!(
                "The same plan, reading the same pages: {details}. Time alone changed, which the cache or the runner's load may explain."
            ));
        }
    }
    let status = if !reasons.is_empty() {
        Status::Failed
    } else if baseline.is_none() {
        Status::New
    } else {
        Status::Passed
    };
    Checked {
        name: name.to_owned(),
        status,
        shape: fingerprint::id(&plan),
        reasons,
        notes,
        diff,
        plan,
        analysis,
        baseline,
        failing,
    }
}

/// The plans a project locks, by name: `explainsql.lock`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    /// The format of the file.
    pub version: u32,
    pub plans: BTreeMap<String, Locked>,
}

/// A locked plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locked {
    /// Its [shape id](fingerprint::id), to see at a glance whether a plan
    /// is still the same.
    pub shape: String,
    /// Pages the statement read, when it was run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    /// The planner's estimated cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// The plan as captured: a JSON plan as JSON, any other form as text.
    pub plan: Value,
}

/// The version of the lock file this code writes.
const LOCK_VERSION: u32 = 1;

impl Default for Lock {
    fn default() -> Self {
        Lock {
            version: LOCK_VERSION,
            plans: BTreeMap::new(),
        }
    }
}

impl Lock {
    /// Reads a lock file.
    pub fn read(text: &str) -> Result<Lock, String> {
        let lock: Lock = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if lock.version != LOCK_VERSION {
            return Err(format!(
                "lock file version {} is not supported; this explainsql reads version {LOCK_VERSION}",
                lock.version
            ));
        }
        Ok(lock)
    }

    /// The lock file's text: pretty JSON, sorted by name, so that its
    /// changes read well in a review.
    pub fn write(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a lock serializes");
        text.push('\n');
        text
    }

    /// Locks a plan under a name, from the text it was read from.
    pub fn lock(&mut self, name: &str, text: &str, plan: &Plan) {
        let captured = match serde_json::from_str::<Value>(text.trim()) {
            Ok(value @ (Value::Array(_) | Value::Object(_))) => value,
            _ => Value::String(text.to_owned()),
        };
        self.plans.insert(
            name.to_owned(),
            Locked {
                shape: fingerprint::id(plan),
                pages: plan
                    .root()
                    .buffers
                    .map(|buffers| crate::metrics::blocks(&buffers)),
                cost: crate::compare::planner_cost(plan),
                plan: captured,
            },
        );
    }

    /// The locked plan of a name, parsed.
    pub fn plan(&self, name: &str) -> Option<Result<Plan, String>> {
        let locked = self.plans.get(name)?;
        let text = match &locked.plan {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        Some(crate::parse(&text).map_err(|error| format!("the locked plan: {error}")))
    }
}

/// Whether every plan passed: none failed.
pub fn passed(checked: &[Checked]) -> bool {
    checked
        .iter()
        .all(|checked| checked.status != Status::Failed)
}

/// `3 plans: 1 failed, 1 new, 1 passed.`
pub fn summary(checked: &[Checked]) -> String {
    let count = |status| {
        checked
            .iter()
            .filter(|checked| checked.status == status)
            .count()
    };
    let mut parts = Vec::new();
    for (status, word) in [
        (Status::Failed, "failed"),
        (Status::New, "new"),
        (Status::Passed, "passed"),
    ] {
        let n = count(status);
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    let plans = if checked.len() == 1 { "plan" } else { "plans" };
    if parts.is_empty() {
        format!("{} {plans}.", checked.len())
    } else {
        format!("{} {plans}: {}.", checked.len(), parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> (Plan, Analysis) {
        let plan = crate::parse(text).unwrap();
        let analysis = crate::analyze(&plan);
        (plan, analysis)
    }

    const SCANNED: &str = "\
Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=20) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.899 ms";

    const INDEXED: &str = "\
Index Scan using orders_customer_id_idx on orders o  (cost=0.42..44.50 rows=10 width=20) (actual time=0.020..0.051 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit=13
Execution Time: 0.070 ms";

    #[test]
    fn findings_fail_a_plan_from_the_severity_asked() {
        let (scanned, analysis) = plan(SCANNED);
        let lenient = check(
            "a.sql",
            scanned.clone(),
            analysis.clone(),
            None,
            Policy::default(),
        );
        assert_eq!(lenient.status, Status::New);
        let strict = Policy {
            fail_on: Some(Severity::High),
            strict: false,
        };
        let failed = check("a.sql", scanned, analysis, None, strict);
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(
            failed.reasons,
            [
                "ES001 Selective sequential scan: Seq Scan on orders o reads 200,000 rows to keep 10."
            ]
        );
        assert!(!passed(&[failed]));
    }

    #[test]
    fn a_plan_worse_than_its_locked_one_fails() {
        let (indexed, _) = plan(INDEXED);
        let (scanned, analysis) = plan(SCANNED);
        let worse = check(
            "a.sql",
            scanned.clone(),
            analysis.clone(),
            Some(indexed.clone()),
            Policy::default(),
        );
        assert_eq!(worse.status, Status::Failed);
        assert_eq!(
            worse.reasons[0],
            "Worse than the locked plan: pages 13 → 2,417 (186× more), execution 0.070 ms → 11.9 ms (170× slower). Index Scan using orders_customer_id_idx on orders o became Seq Scan on orders o."
        );
        // Better: the plan changed, which is worth a note.
        let (_, indexed_analysis) = plan(INDEXED);
        let better = check(
            "a.sql",
            indexed.clone(),
            indexed_analysis.clone(),
            Some(scanned),
            Policy::default(),
        );
        assert_eq!(better.status, Status::Passed);
        assert!(
            better.notes[0].starts_with("The plan changed, better: pages 2,417 → 13"),
            "{}",
            better.notes[0]
        );
        // Unless the plan must stay the same.
        let changed = check(
            "a.sql",
            indexed.clone(),
            indexed_analysis.clone(),
            Some(plan(SCANNED).0),
            Policy {
                fail_on: None,
                strict: true,
            },
        );
        assert_eq!(changed.status, Status::Failed);
        // The same plan passes.
        let same = check(
            "a.sql",
            indexed.clone(),
            indexed_analysis,
            Some(indexed),
            Policy::default(),
        );
        assert_eq!(same.status, Status::Passed);
        assert!(same.reasons.is_empty() && same.notes.is_empty());
    }

    #[test]
    fn time_alone_does_not_fail_a_plan() {
        let (before, _) = plan(INDEXED);
        let (slower, analysis) = plan(&INDEXED.replace("0.051", "0.510").replace("0.070", "0.700"));
        let checked = check("a.sql", slower, analysis, Some(before), Policy::default());
        assert_eq!(checked.status, Status::Passed);
        assert!(
            checked.notes[0].starts_with("The same plan, reading the same pages: pages 13 → 13"),
            "{}",
            checked.notes[0]
        );
    }

    #[test]
    fn locks_plans() {
        let mut lock = Lock::default();
        let (scanned, _) = plan(SCANNED);
        lock.lock("queries/a.sql", SCANNED, &scanned);
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t", "Alias": "t", "Startup Cost": 0.0, "Total Cost": 1.0, "Plan Rows": 1, "Plan Width": 4}}]"#;
        let (from_json, _) = plan(json);
        lock.lock("queries/b.sql", json, &from_json);
        let text = lock.write();
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"plans\": {\n    \"queries/a.sql\": {"));
        // JSON plans stay JSON, so that their changes read well in a review.
        assert!(text.contains("\"Node Type\": \"Seq Scan\""));
        let read = Lock::read(&text).unwrap();
        assert_eq!(read, lock);
        assert_eq!(read.plans["queries/a.sql"].pages, Some(2417));
        assert_eq!(read.plans["queries/b.sql"].cost, Some(1.0));
        assert_eq!(read.plan("queries/a.sql").unwrap().unwrap(), scanned);
        assert_eq!(read.plan("queries/b.sql").unwrap().unwrap(), from_json);
        assert!(read.plan("queries/c.sql").is_none());
        // Other versions are refused rather than misread.
        assert!(Lock::read(&text.replace("\"version\": 1", "\"version\": 2")).is_err());
        assert!(Lock::read("not json").is_err());
    }

    #[test]
    fn sums_up() {
        let (indexed, analysis) = plan(INDEXED);
        let new = check(
            "a.sql",
            indexed.clone(),
            analysis.clone(),
            None,
            Policy::default(),
        );
        let same = check(
            "b.sql",
            indexed.clone(),
            analysis,
            Some(indexed),
            Policy::default(),
        );
        assert_eq!(summary(&[new.clone(), same]), "2 plans: 1 new, 1 passed.");
        assert_eq!(summary(&[new]), "1 plan: 1 new.");
    }
}
