//! Why the planner chose its plan, found by asking it again. The statement
//! is planned with the choice taken away (`enable_seqscan = off` for a
//! sequential scan, `enable_nestloop = off` for a nested loop), or with
//! enough work_mem for a sort or hash that spilled to disk, and the plans
//! are compared:
//!
//! - An index the planner cannot use even with sequential scans off cannot
//!   serve the condition, and the condition and the catalog say why.
//! - Otherwise the planner's estimates say by how much it preferred its
//!   plan, and measuring both plans says whether it was right.
//! - When the alternative is better, a row misestimate or the cost settings
//!   explain the wrong choice. Planning once more with `random_page_cost =
//!   1.1` tells the second apart: the planner then picks the alternative by
//!   itself.
//!
//! `enable_*` settings hold for the whole statement, so other parts of the
//! plan can change too; the answer says so. This module decides what to ask
//! and what the plans mean. `explainsql-db` plans and runs the statement,
//! always in transactions that are rolled back.

use serde::Serialize;
use serde_json::Value;

use crate::advisor::{Advice, AdviceKind};
use crate::analysis::Analysis;
use crate::catalog::{Catalog, ExistingIndex, Table};
use crate::compare::{self, Change, Comparison};
use crate::expr::{self, Access};
use crate::fingerprint::{self, Relation};
use crate::format;
use crate::ir::{Node, NodeId, Plan, PredicateKind, Relationship};
use crate::metrics::{self, Metrics, Misestimate};
use crate::rules::{Evidence, qualifier};
use crate::scenario::{self, Setting};

/// Share of the runtime from which a node is worth asking about.
const MIN_SHARE: f64 = 0.1;
/// How many nodes to ask about when none is named: each question plans the
/// statement again, and measuring runs it.
const MAX_QUESTIONS: usize = 3;
/// A row misestimate from this factor explains a wrong choice.
const MISESTIMATE: f64 = 10.0;
/// random_page_cost for storage that reads at random about as fast as in
/// sequence: SSDs, cloud volumes, a cache that holds the table.
const FAST_RANDOM_PAGE_COST: &str = "1.1";
const DEFAULT_RANDOM_PAGE_COST: f64 = 4.0;
/// work_mem, in kilobytes: PostgreSQL's default, and the most suggested.
const DEFAULT_WORK_MEM: u64 = 4 * 1024;
const MAX_WORK_MEM: u64 = 1024 * 1024;
/// A plan within this fraction of the planner's choice is a close call.
const CLOSE_CALL: f64 = 0.1;

/// Which nodes to ask about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The slowest sequential scans, nested loops and, when measuring,
    /// spills, up to three.
    Hotspots,
    Node(NodeId),
    /// The sequential scans of a table, named by its name or alias, or of
    /// the table an index belongs to.
    Name(String),
}

impl Target {
    /// From what was typed after `--why-not`: nothing for the hotspots.
    pub fn parse(text: &str) -> Target {
        let text = text.trim();
        if text.is_empty() {
            Target::Hotspots
        } else {
            // A schema-qualified name: plans name relations without it.
            Target::Name(text.rsplit('.').next().unwrap_or(text).to_owned())
        }
    }
}

/// Something to ask the planner about one node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Question {
    pub node: NodeId,
    #[serde(flatten)]
    pub topic: Topic,
    /// The question, as reports print it.
    pub text: String,
    /// The settings that take the planner's choice away, or give the
    /// operation more memory.
    pub settings: Vec<Setting>,
    /// Cost settings to plan with as well, to see whether the planner would
    /// choose the alternative by itself.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cost_settings: Vec<Setting>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum Topic {
    /// Why a sequential scan of the relation rather than an index; about
    /// one index, when it was named.
    Index {
        relation: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        index: Option<String>,
    },
    /// Why a nested loop rather than a hash or merge join.
    NestedLoop,
    /// Whether the operation stays in memory, and the statement gets
    /// better, with more work_mem.
    Memory { work_mem: String },
}

/// What the database returned for a question.
#[derive(Debug, Clone, Copy)]
pub struct Evaluation<'a> {
    /// Measured runs of the statement as the planner chose it, by the same
    /// protocol as the alternative's; empty when only estimating.
    pub chosen: &'a [Plan],
    /// The plan under the question's settings: estimated, or each measured
    /// run.
    pub alternative: &'a [Plan],
    /// The estimated plan under the question's cost settings, if it has any.
    pub with_cost_settings: Option<&'a Plan>,
    /// Measured runs under the cost settings, when the planner picks
    /// another plan with them ([`measure_cost_settings`]); empty otherwise.
    pub cost_settings_runs: &'a [Plan],
    pub catalog: Option<&'a Catalog>,
}

/// Whether to measure the statement under the question's cost settings as
/// well: only when they make the planner leave the sequential scan, since a
/// setting is suggested only once the plan it leads to is measured better.
pub fn measure_cost_settings(question: &Question, with_cost_settings: &Plan) -> bool {
    let Topic::Index {
        relation, alias, ..
    } = &question.topic
    else {
        return false;
    };
    let relation = Relation {
        name: relation.clone(),
        alias: alias.clone(),
    };
    fingerprint::find_scan(with_cost_settings, &relation).is_some_and(|scan| {
        !fingerprint::indexes_with_condition(with_cost_settings, scan).is_empty()
    })
}

/// The planner's answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Answer {
    pub node: NodeId,
    #[serde(flatten)]
    pub topic: Topic,
    pub question: String,
    pub verdict: Verdict,
    /// The answer, in a sentence or two.
    pub summary: String,
    pub evidence: Vec<Evidence>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The settings the alternative was planned with.
    pub settings: Vec<Setting>,
    /// Measured with EXPLAIN ANALYZE rather than estimated.
    pub measured: bool,
    /// The settings changed other parts of the plan too, so the comparison
    /// covers more than this node.
    pub approximate: bool,
    /// The planner's choice against the alternative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The planner cannot use the alternative at all.
    Unusable,
    /// Estimated only: the planner found the alternative more expensive.
    /// Measuring tells whether it is right.
    Costlier,
    /// Measured: the alternative is not better.
    PlannerRight,
    /// Measured: the alternative is better; the planner misjudged how many
    /// rows it would get.
    Misestimate,
    /// Measured: the alternative is better, and with random_page_cost = 1.1
    /// the planner chooses it by itself.
    CostSettings,
    /// Measured: the alternative is better, for a reason not found.
    PlannerWrong,
    /// More work_mem keeps the operation in memory and the statement gets
    /// better.
    MoreMemoryHelps,
    /// More work_mem does not make the statement better.
    MoreMemoryDoesNotHelp,
    /// The plans do not say: they compare both ways, or could not be matched.
    Inconclusive,
}

impl Verdict {
    /// A short tag for lists: `UNUSABLE`, `COST MODEL`.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Unusable => "UNUSABLE",
            Verdict::Costlier => "COSTLIER",
            Verdict::PlannerRight => "RIGHT",
            Verdict::Misestimate => "MISESTIMATE",
            Verdict::CostSettings => "COST MODEL",
            Verdict::PlannerWrong => "WRONG",
            Verdict::MoreMemoryHelps => "HELPS",
            Verdict::MoreMemoryDoesNotHelp => "NO HELP",
            Verdict::Inconclusive => "UNSURE",
        }
    }

    /// The verdict in words.
    pub fn describe(self) -> &'static str {
        match self {
            Verdict::Unusable => "no index can serve the condition",
            Verdict::Costlier => "the planner estimates the alternative more expensive",
            Verdict::PlannerRight => "the planner's choice is the better one",
            Verdict::Misestimate => "the planner chose wrongly because of a row misestimate",
            Verdict::CostSettings => {
                "the planner chose wrongly because of its cost settings (random_page_cost)"
            }
            Verdict::PlannerWrong => "the planner chose wrongly",
            Verdict::MoreMemoryHelps => "more work_mem helps",
            Verdict::MoreMemoryDoesNotHelp => "more work_mem does not help",
            Verdict::Inconclusive => "the plans do not say",
        }
    }
}

/// What to ask about the plan: the nodes the target names, or the slowest
/// ones worth asking about. Questions about memory need measured runs, so
/// they come only with `measure`.
pub fn questions(
    plan: &Plan,
    analysis: &Analysis,
    catalog: Option<&Catalog>,
    target: &Target,
    measure: bool,
) -> Vec<Question> {
    match target {
        Target::Node(id) if id.index() < plan.nodes.len() => {
            question(plan, analysis, *id, None, measure, true)
                .into_iter()
                .collect()
        }
        Target::Node(_) => Vec::new(),
        Target::Name(name) => {
            let (relation, index) = resolve(plan, catalog, name);
            plan.nodes
                .iter()
                .filter(|node| {
                    node.node_type == "Seq Scan"
                        && (node.relation_name.as_deref() == Some(relation.as_str())
                            || node.alias.as_deref() == Some(relation.as_str()))
                })
                .filter_map(|node| question(plan, analysis, node.id, index.clone(), measure, true))
                .collect()
        }
        Target::Hotspots => {
            let mut candidates: Vec<(NodeId, f64)> = plan
                .nodes
                .iter()
                .filter_map(|node| Some((node.id, hot_share(plan, analysis, node, measure)?)))
                .filter(|&(_, share)| share >= MIN_SHARE)
                .collect();
            candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            candidates
                .into_iter()
                .filter_map(|(id, _)| question(plan, analysis, id, None, measure, false))
                .take(MAX_QUESTIONS)
                .collect()
        }
    }
}

/// The share of the runtime a node stands for, if it is the kind of node
/// worth asking about: a filtered sequential scan, a nested loop that
/// repeats an expensive inner side or follows a misestimate, a spill.
fn hot_share(plan: &Plan, analysis: &Analysis, node: &Node, measure: bool) -> Option<f64> {
    let metrics = analysis.metrics.node(node.id);
    match node.node_type.as_str() {
        "Seq Scan" if !conditions(plan, node).is_empty() => metrics.time_share.or_else(|| {
            let total = metrics::blocks(&plan.root().buffers?);
            let own = metrics::blocks(&metrics.exclusive_buffers?);
            (total > 0).then(|| own as f64 / total as f64)
        }),
        "Nested Loop" => {
            let flagged = analysis
                .findings
                .iter()
                .any(|finding| finding.rule.id == "ES005" && finding.node == Some(node.id));
            let misled = plan.children(node.id).any(|child| {
                child.relationship == Some(Relationship::Outer)
                    && analysis
                        .metrics
                        .node(child.id)
                        .misestimate
                        .is_some_and(|error| error.underestimated && error.factor >= MISESTIMATE)
            });
            (flagged || misled).then(|| inclusive_share(analysis, node))?
        }
        _ if measure && spill_needs(node).is_some() => inclusive_share(analysis, node),
        _ => None,
    }
}

fn inclusive_share(analysis: &Analysis, node: &Node) -> Option<f64> {
    let total = analysis.metrics.statement.total_time?;
    Some(analysis.metrics.node(node.id).inclusive_time? / total)
}

/// The relation a name stands for, and the index, when it names one.
fn resolve(plan: &Plan, catalog: Option<&Catalog>, name: &str) -> (String, Option<String>) {
    let in_plan = plan.nodes.iter().any(|node| {
        node.relation_name.as_deref() == Some(name) || node.alias.as_deref() == Some(name)
    });
    if in_plan {
        return (name.to_owned(), None);
    }
    let used = plan
        .nodes
        .iter()
        .find(|node| node.index_name.as_deref() == Some(name))
        .and_then(|node| node.relation_name.clone());
    let known = || {
        catalog?
            .tables
            .iter()
            .find(|table| table.indexes.iter().any(|index| index.name == name))
            .map(|table| table.name.clone())
    };
    match used.or_else(known) {
        Some(table) => (table, Some(name.to_owned())),
        None => (name.to_owned(), None),
    }
}

fn question(
    plan: &Plan,
    analysis: &Analysis,
    id: NodeId,
    index: Option<String>,
    measure: bool,
    named: bool,
) -> Option<Question> {
    let node = plan.node(id);
    let label = format::node(node);
    match node.node_type.as_str() {
        "Seq Scan" => {
            let relation = Relation::of(node)?;
            let mut cost_settings = Vec::new();
            if random_page_cost(plan) > 1.1 {
                cost_settings.push(Setting::new("random_page_cost", FAST_RANDOM_PAGE_COST));
            }
            Some(Question {
                node: id,
                text: match &index {
                    Some(index) => format!("Why does {label} not use {index}?"),
                    None => format!("Why does {label} not use an index?"),
                },
                topic: Topic::Index {
                    relation: relation.name,
                    alias: relation.alias,
                    index,
                },
                settings: vec![Setting::new("enable_seqscan", "off")],
                cost_settings,
            })
        }
        "Nested Loop" if named || hot_share(plan, analysis, node, measure).is_some() => {
            Some(Question {
                node: id,
                topic: Topic::NestedLoop,
                text: format!(
                    "Why does the join of {} run as a nested loop rather than a hash or merge join?",
                    joined(plan, id)
                ),
                settings: vec![Setting::new("enable_nestloop", "off")],
                cost_settings: Vec::new(),
            })
        }
        _ if measure => {
            let work_mem = memory(plan, node)?;
            Some(Question {
                node: id,
                topic: Topic::Memory {
                    work_mem: work_mem.clone(),
                },
                text: format!("Would {label} stay in memory with work_mem = {work_mem}?"),
                settings: vec![Setting::new("work_mem", &work_mem)],
                cost_settings: Vec::new(),
            })
        }
        _ => None,
    }
}

/// The relations a join combines, as a plan names them: `orders o and
/// order_items oi`.
fn joined(plan: &Plan, id: NodeId) -> String {
    let names: Vec<String> = fingerprint::relations(plan, id)
        .into_iter()
        .map(|relation| match relation.alias {
            Some(alias) if alias != relation.name => format!("{} {alias}", relation.name),
            _ => relation.name,
        })
        .collect();
    match names.as_slice() {
        [] => "its inputs".to_owned(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, rest @ ..] => format!("{first}, {second} and {} more", rest.len()),
    }
}

/// A planner setting as the plan reports it (SETTINGS lists the ones that
/// differ from their defaults).
fn setting<'a>(plan: &'a Plan, name: &str) -> Option<&'a str> {
    plan.summary.settings.get(name).map(String::as_str)
}

fn random_page_cost(plan: &Plan) -> f64 {
    setting(plan, "random_page_cost")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_RANDOM_PAGE_COST)
}

/// How much memory a spilled operation would need to stay in memory, in
/// kilobytes, by what the plan shows.
fn spill_needs(node: &Node) -> Option<f64> {
    let number = |extra: &std::collections::BTreeMap<String, Value>, key: &str| {
        extra.get(key).and_then(Value::as_f64)
    };
    match node.node_type.as_str() {
        "Sort" => {
            // On disk, sorted rows take about a third of the room they
            // take in memory.
            let disk = std::iter::once(&node.extra)
                .chain(node.workers.iter().map(|worker| &worker.extra))
                .filter(|extra| {
                    extra.get("Sort Space Type").and_then(Value::as_str) == Some("Disk")
                })
                .filter_map(|extra| number(extra, "Sort Space Used"))
                .fold(0.0, f64::max);
            (disk > 0.0).then_some(disk * 3.0)
        }
        "Hash" => {
            let batches = node
                .extra_f64("Hash Batches")
                .filter(|&batches| batches > 1.0)?;
            // Each batch held about as much as the one in memory.
            Some(node.extra_f64("Peak Memory Usage")? * batches * 1.25)
        }
        "Aggregate" if matches!(node.strategy.as_deref(), Some("Hashed" | "Mixed")) => {
            let disk = node.extra_f64("Disk Usage").unwrap_or(0.0);
            let batches = node.extra_f64("HashAgg Batches").unwrap_or(0.0);
            (disk > 0.0 || batches > 1.0)
                .then(|| (node.extra_f64("Peak Memory Usage").unwrap_or(0.0) + disk) * 2.0)
        }
        _ => None,
    }
}

/// The work_mem to try for a spilled operation: a power of two megabytes,
/// more than the plan ran with, and at most a gigabyte.
fn memory(plan: &Plan, node: &Node) -> Option<String> {
    let needed = spill_needs(node)?;
    let current = setting(plan, "work_mem")
        .and_then(scenario::kilobytes)
        .unwrap_or(DEFAULT_WORK_MEM);
    let mut megabytes: u64 = 1;
    // Kilobytes in the range of u64 are exact enough as floats here.
    #[allow(clippy::cast_precision_loss)]
    while ((megabytes * 1024) as f64) < needed && megabytes * 1024 < MAX_WORK_MEM {
        megabytes *= 2;
    }
    let kilobytes = megabytes * 1024;
    #[allow(clippy::cast_precision_loss)]
    let enough = kilobytes as f64 >= needed;
    (enough && kilobytes > current).then(|| {
        if megabytes >= 1024 {
            format!("{}GB", megabytes / 1024)
        } else {
            format!("{megabytes}MB")
        }
    })
}

/// The conditions an index would have to serve: the scan's filter and, on
/// the inner side of a nested loop, the loop's join filter.
fn conditions<'a>(plan: &'a Plan, scan: &'a Node) -> Vec<&'a str> {
    let mut conditions: Vec<&str> = scan.predicate(PredicateKind::Filter).into_iter().collect();
    if scan.relationship == Some(Relationship::Inner) {
        if let Some(parent) = scan
            .parent
            .map(|id| plan.node(id))
            .filter(|parent| parent.node_type == "Nested Loop")
        {
            conditions.extend(parent.predicate(PredicateKind::JoinFilter));
        }
    }
    conditions
}

/// The answer the plans give to a question about `plan`, the statement as
/// the planner chose it.
pub fn answer(
    plan: &Plan,
    analysis: &Analysis,
    question: &Question,
    evaluation: &Evaluation,
) -> Answer {
    let measured = !evaluation.chosen.is_empty()
        && evaluation
            .alternative
            .first()
            .is_some_and(|alternative| alternative.root().actuals.is_some());
    let mut answer = Answer {
        node: question.node,
        topic: question.topic.clone(),
        question: question.text.clone(),
        verdict: Verdict::Inconclusive,
        summary: String::new(),
        evidence: Vec::new(),
        action: None,
        settings: question.settings.clone(),
        measured,
        approximate: false,
        comparison: None,
    };
    let Some(alternative) = evaluation.alternative.first() else {
        answer.summary = "The database returned no plan to compare with.".to_owned();
        return answer;
    };
    // Misestimates show in measured runs: those of the comparison, or the
    // plan itself when it was measured.
    let computed;
    let (run, run_metrics) = match evaluation.chosen.first() {
        Some(run) => {
            computed = metrics::compute(run);
            (run, &computed)
        }
        None => (plan, &analysis.metrics),
    };
    let context = Context {
        plan,
        alternative,
        run,
        run_metrics,
        evaluation,
    };
    match &question.topic {
        Topic::Index {
            relation,
            alias,
            index,
        } => {
            let relation = Relation {
                name: relation.clone(),
                alias: alias.clone(),
            };
            index_answer(&context, question, &relation, index.as_deref(), &mut answer);
        }
        Topic::NestedLoop => join_answer(&context, question, &mut answer),
        Topic::Memory { work_mem } => memory_answer(&context, question, work_mem, &mut answer),
    }
    answer
}

/// The answer when the alternative ran past the statement timeout while
/// the planner's choice finished: the alternative is the slower plan.
pub fn timed_out(question: &Question, timeout: &str) -> Answer {
    let settings: Vec<String> = question.settings.iter().map(ToString::to_string).collect();
    let (verdict, action) = match question.topic {
        Topic::Memory { .. } => (Verdict::MoreMemoryDoesNotHelp, "Leave work_mem as it is."),
        _ => (
            Verdict::PlannerRight,
            "Nothing to change in the planner: the alternative is far slower.",
        ),
    };
    Answer {
        node: question.node,
        topic: question.topic.clone(),
        question: question.text.clone(),
        verdict,
        summary: format!(
            "With {} the statement ran past the {timeout} timeout and was stopped, while the planner's choice finished.",
            settings.join(", ")
        ),
        evidence: Vec::new(),
        action: Some(action.to_owned()),
        settings: question.settings.clone(),
        measured: true,
        approximate: false,
        comparison: None,
    }
}

/// The answer when the alternative could not be planned or run.
pub fn failed(question: &Question, error: &str) -> Answer {
    Answer {
        node: question.node,
        topic: question.topic.clone(),
        question: question.text.clone(),
        verdict: Verdict::Inconclusive,
        summary: format!("The alternative could not be planned or run: {error}."),
        evidence: Vec::new(),
        action: None,
        settings: question.settings.clone(),
        measured: false,
        approximate: false,
        comparison: None,
    }
}

/// Everything an answer looks at.
struct Context<'a> {
    /// The statement as the planner chose it.
    plan: &'a Plan,
    /// The first plan under the question's settings.
    alternative: &'a Plan,
    /// A measured run of the planner's choice, or the plan itself, and its
    /// metrics: where misestimates show.
    run: &'a Plan,
    run_metrics: &'a Metrics,
    evaluation: &'a Evaluation<'a>,
}

impl Context<'_> {
    /// The planner's choice against the alternative: measured runs when
    /// there are, else the estimates.
    fn comparison(&self, measured: bool) -> Comparison {
        if measured {
            compare::compare_runs(self.evaluation.chosen, self.evaluation.alternative)
        } else {
            compare::compare(self.plan, self.alternative)
        }
    }

    /// How a node of the planner's choice misjudged its rows, in the
    /// measured run.
    fn misestimate(&self, relation_or_node: Found) -> Option<(&Node, Misestimate)> {
        let node = match relation_or_node {
            Found::Scan(relation) => fingerprint::find_scan(self.run, relation)?,
            // The same node in the measured run: the same kind, over the
            // same relations.
            Found::Node(id) => {
                let wanted = self.plan.node(id);
                let relations = fingerprint::relations(self.plan, id);
                self.run.nodes.iter().find(|node| {
                    node.node_type == wanted.node_type
                        && fingerprint::relations(self.run, node.id) == relations
                })?
            }
        };
        let error = self.run_metrics.node(node.id).misestimate?;
        Some((node, error))
    }

    /// Scans and joins outside the node asked about that the settings
    /// changed too, in plan order: `Nested Loop (was Hash Join)`.
    fn changed_elsewhere(&self, target: NodeId) -> Vec<String> {
        let mine = fingerprint::relations(self.plan, target);
        let mut changed = Vec::new();
        for node in &self.plan.nodes {
            if node.id == target {
                continue;
            }
            let other = match Relation::of(node) {
                Some(relation) if mine.contains(&relation) => continue,
                Some(relation) => fingerprint::find_scan(self.alternative, &relation)
                    .filter(|other| access(self.plan, node) != access(self.alternative, other)),
                None if fingerprint::is_join(node) => {
                    let relations = fingerprint::relations(self.plan, node.id);
                    if relations.is_subset(&mine) {
                        continue;
                    }
                    fingerprint::find_join(self.alternative, &relations)
                        .filter(|other| other.node_type != node.node_type)
                }
                None => None,
            };
            if let Some(other) = other {
                changed.push(format!(
                    "{} (was {})",
                    format::node(other),
                    format::node(node)
                ));
            }
        }
        changed
    }

    /// Notes that the settings changed more than the node asked about.
    fn note_changes(&self, target: NodeId, answer: &mut Answer) {
        let changed = self.changed_elsewhere(target);
        if changed.is_empty() {
            return;
        }
        answer.approximate = true;
        let mut text = changed
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if changed.len() > 3 {
            text.push_str(&format!("; and {} more", changed.len() - 3));
        }
        answer.evidence.push(evidence("Also changed", text));
    }
}

/// What to look for in a measured run.
#[derive(Clone, Copy)]
enum Found<'a> {
    Scan(&'a Relation),
    Node(NodeId),
}

/// How a scan reads its relation, to compare between plans.
fn access(plan: &Plan, node: &Node) -> (String, Vec<String>) {
    (
        node.node_type.clone(),
        fingerprint::indexes_with_condition(plan, node),
    )
}

fn evidence(label: &'static str, value: impl Into<String>) -> Evidence {
    Evidence {
        label,
        value: value.into(),
    }
}

/// How the alternative's cost compares with the planner's choice, and in
/// words: `a close call: 5,210 against 4,917, within 10%`.
fn cost_comparison(chosen: Option<f64>, alternative: Option<f64>) -> Option<(f64, String)> {
    let (chosen, alternative) = (chosen?, alternative?);
    if chosen <= 0.0 {
        return None;
    }
    let increase = alternative / chosen - 1.0;
    let text = if increase.abs() < CLOSE_CALL {
        format!(
            "a close call: {} against {}, within 10%",
            format::rows(alternative.round()),
            format::rows(chosen.round())
        )
    } else if increase < 1.0 {
        format!(
            "{} more expensive: {} against {}",
            format::percent(increase),
            format::rows(alternative.round()),
            format::rows(chosen.round())
        )
    } else {
        format!(
            "{} as expensive: {} against {}",
            format::factor(alternative / chosen),
            format::rows(alternative.round()),
            format::rows(chosen.round())
        )
    };
    Some((increase, text))
}

fn index_answer(
    context: &Context,
    question: &Question,
    relation: &Relation,
    named: Option<&str>,
    answer: &mut Answer,
) {
    let plan = context.plan;
    let scan = plan.node(question.node);
    let label = format::node(scan);
    let conditions = conditions(plan, scan);
    let table = context
        .evaluation
        .catalog
        .and_then(|catalog| catalog.table(scan.schema.as_deref(), &relation.name));
    let Some(other) = fingerprint::find_scan(context.alternative, relation) else {
        answer.summary = format!(
            "With sequential scans off, the plan no longer reads {} the same way, so the scans cannot be compared.",
            relation.name
        );
        return;
    };
    context.note_changes(question.node, answer);
    let indexes = fingerprint::indexes_with_condition(context.alternative, other);
    let whole_index = fingerprint::reads_whole_index(other);
    let still_scans = other.node_type == "Seq Scan";
    if indexes.is_empty() && (still_scans || (whole_index && !conditions.is_empty())) {
        answer.verdict = Verdict::Unusable;
        let reasons = unusable_reasons(scan, &conditions, table, named);
        answer.summary = if still_scans {
            format!(
                "No index can serve the condition of {label}: even with sequential scans off, the planner still reads all of {}.",
                relation.name
            )
        } else {
            format!(
                "No index can serve the condition of {label}: with sequential scans off, the planner reads all of {} instead, without a condition.",
                other.index_name.as_deref().unwrap_or("an index")
            )
        };
        for reason in &reasons {
            answer.evidence.push(evidence("Why", reason.clone()));
        }
        answer.action = Some(unusable_action(&reasons));
        return;
    }
    let path = format::node(other);
    // What the planner picks with the cost settings: an index, or the scan.
    let by_itself: Option<Option<String>> = context.evaluation.with_cost_settings.map(|with| {
        fingerprint::find_scan(with, relation)
            .filter(|scan| !fingerprint::indexes_with_condition(with, scan).is_empty())
            .map(format::node)
    });
    let current = random_page_cost(plan);
    let comparison = context.comparison(answer.measured);
    answer
        .evidence
        .push(evidence("Planner's choice", plan_cost(&label, plan)));
    answer.evidence.push(evidence(
        "Alternative",
        plan_cost(&path, context.alternative),
    ));
    if let Some(by_itself) = &by_itself {
        answer.evidence.push(evidence(
            "With random_page_cost = 1.1",
            match by_itself {
                Some(picked) => format!("the planner chooses {picked} by itself"),
                None => "the planner keeps the sequential scan".to_owned(),
            },
        ));
    }
    if !answer.measured {
        answer.verdict = Verdict::Costlier;
        let estimate = cost_comparison(
            compare::planner_cost(plan),
            compare::planner_cost(context.alternative),
        );
        answer.summary = match &estimate {
            Some((increase, text)) if *increase < CLOSE_CALL => format!(
                "PostgreSQL can use {path}, and estimates it about as expensive as the sequential scan ({text}). A small change in the statistics or the cost settings can flip this plan."
            ),
            Some((_, text)) => format!(
                "PostgreSQL can use {path}, but estimates it {text}, so it chose the sequential scan."
            ),
            None => format!("PostgreSQL can use {path}, but estimates it more expensive."),
        };
        answer.action = Some(
            "Measure both plans to know whether the planner is right: --measure, or y in a viewer started with --measure.".to_owned(),
        );
        answer.comparison = Some(comparison);
        return;
    }
    let overestimate = context
        .misestimate(Found::Scan(relation))
        .filter(|(_, error)| !error.underestimated && error.factor >= MISESTIMATE);
    match comparison.change {
        Change::Better => {
            if let Some((node, error)) = overestimate {
                answer.verdict = Verdict::Misestimate;
                let rows = rows_line(node, error);
                answer.summary = format!(
                    "With {path} the statement is better ({}). The planner chose the sequential scan because it expected {}.",
                    comparison.details(),
                    rows
                );
                answer.evidence.push(evidence("Row estimate", rows));
                answer.action = Some(format!(
                    "Fix the estimate: ANALYZE {}; if it stays off, raise the statistics target of the filtered columns or CREATE STATISTICS on them (see ES002).",
                    relation.name
                ));
            } else if let (Some(Some(picked)), Some(confirmation)) =
                (&by_itself, cost_settings_comparison(context))
            {
                // The plan the cost settings lead to, measured: a setting
                // is suggested only when that plan is better too.
                let picked = if *picked == path {
                    "it".to_owned()
                } else {
                    picked.clone()
                };
                if confirmation.change == Change::Better {
                    answer.verdict = Verdict::CostSettings;
                    answer.summary = format!(
                        "With {path} the statement is better ({}). With random_page_cost = 1.1 the planner chooses {picked} by itself, and that is better as well ({}). At {} it prices a page read at random {} as a page read in sequence, too much for SSDs, cloud volumes and tables that stay cached.",
                        comparison.details(),
                        confirmation.details(),
                        format_cost(current),
                        if (current - DEFAULT_RANDOM_PAGE_COST).abs() < f64::EPSILON {
                            "four times as expensive".to_owned()
                        } else {
                            format!("{} as expensive", format::factor(current))
                        }
                    );
                    answer.action = Some(
                        "Set random_page_cost = 1.1 where the storage reads at random about as fast as in sequence: ALTER DATABASE … SET random_page_cost = 1.1, or for the tablespace or a role. It changes the plans of every statement there, so compare the slowest ones before and after.".to_owned(),
                    );
                } else {
                    answer.verdict = Verdict::PlannerWrong;
                    answer.summary = format!(
                        "With {path} the statement is better ({}), though the planner did not expect too many rows from the scan. With random_page_cost = 1.1 the planner chooses {picked}, but that is {} ({}), so lowering random_page_cost alone does not fix this plan.",
                        comparison.details(),
                        confirmation.describe(),
                        confirmation.details()
                    );
                    answer.action = Some(
                        "Check effective_cache_size, and how the table's order follows the index (pg_stats.correlation). Until then, a hint (pg_hint_plan) or, from PostgreSQL 19, pg_plan_advice can keep the better plan.".to_owned(),
                    );
                }
            } else {
                answer.verdict = Verdict::PlannerWrong;
                answer.summary = format!(
                    "With {path} the statement is better ({}), though the planner did not expect too many rows from the scan{}.",
                    comparison.details(),
                    if matches!(by_itself, Some(None)) {
                        " and random_page_cost = 1.1 does not change its choice"
                    } else {
                        ""
                    }
                );
                answer.action = Some(
                    "Check effective_cache_size, and how the table's order follows the index (pg_stats.correlation). Until then, a hint (pg_hint_plan) or, from PostgreSQL 19, pg_plan_advice can keep the better plan.".to_owned(),
                );
            }
        }
        Change::Worse | Change::Same => {
            answer.verdict = Verdict::PlannerRight;
            answer.summary = format!(
                "The planner is right: with {path} the statement is {} ({}).",
                comparison.describe(),
                comparison.details()
            );
            answer.action = Some(
                "Nothing to change in the planner: with the indexes there are, the sequential scan is the better plan.".to_owned(),
            );
        }
        Change::Mixed | Change::Unknown => inconclusive(&comparison, answer),
    }
    answer.comparison = Some(comparison);
}

/// The planner's choice against the plan the cost settings lead to, when
/// both were measured.
fn cost_settings_comparison(context: &Context) -> Option<Comparison> {
    let runs = context.evaluation.cost_settings_runs;
    (!runs.is_empty() && !context.evaluation.chosen.is_empty())
        .then(|| compare::compare_runs(context.evaluation.chosen, runs))
}

/// `50,000 rows from Seq Scan on orders where 10 came (5,000× fewer)`.
fn rows_line(node: &Node, error: Misestimate) -> String {
    let (estimated, actual) = (
        node.estimates.map_or(0.0, |estimates| estimates.rows),
        node.actuals.map_or(0.0, |actuals| actuals.rows),
    );
    format!(
        "{} {} from {} where {} came ({} {})",
        format::rows(estimated),
        if estimated == 1.0 { "row" } else { "rows" },
        format::node(node),
        format::rows(actual),
        format::factor(error.factor),
        if error.underestimated {
            "more"
        } else {
            "fewer"
        }
    )
}

/// `Seq Scan on orders; statement cost 4,917`.
fn plan_cost(label: &str, plan: &Plan) -> String {
    match compare::planner_cost(plan) {
        Some(cost) => format!("{label}; statement cost {}", format::rows(cost.round())),
        None => label.to_owned(),
    }
}

fn format_cost(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

fn inconclusive(comparison: &Comparison, answer: &mut Answer) {
    answer.verdict = Verdict::Inconclusive;
    answer.summary = format!(
        "The measurements do not say: the alternative is {} ({}).",
        comparison.describe(),
        comparison.details()
    );
    answer.action =
        Some("Measure again with more runs, such as --runs 5, so that medians compare.".to_owned());
}

/// Why no index serves the conditions, from the conditions themselves and,
/// when connected, the table's indexes and columns.
fn unusable_reasons(
    scan: &Node,
    conditions: &[&str],
    table: Option<&Table>,
    named: Option<&str>,
) -> Vec<String> {
    let own = qualifier(scan);
    let mut reasons: Vec<String> = Vec::new();
    if conditions.is_empty() {
        return vec!["the scan has no condition: the query needs every row".to_owned()];
    }
    for condition in conditions {
        for conjunct in expr::conjuncts(condition) {
            match expr::access(conjunct) {
                Access::Wrapped { column, wrapper } if belongs(column, own) => {
                    let name = expr::split_column(column).1;
                    let typed = table
                        .and_then(|table| table.column(name))
                        .and_then(|column| column.type_name.as_deref())
                        .map(|type_name| format!(" ({type_name})"))
                        .unwrap_or_default();
                    let what = if wrapper == "a cast" {
                        "casts".to_owned()
                    } else {
                        format!("applies {wrapper}() to")
                    };
                    let leading = table.map(|table| leading_names(table, name)).unwrap_or_default();
                    let index = if leading.is_empty() {
                        format!("an index on {name}")
                    } else {
                        leading.join(", ")
                    };
                    reasons.push(format!(
                        "the condition {what} {name}{typed}, so {index} cannot serve it"
                    ));
                }
                Access::OrAcrossColumns => reasons.push(
                    "the filter ORs conditions on different columns, which no single index serves; an index on each column lets PostgreSQL combine them with a BitmapOr"
                        .to_owned(),
                ),
                Access::Column {
                    column,
                    operator,
                    value,
                } if belongs(column, own) => {
                    let name = expr::split_column(column).1;
                    if let Some(reason) = operator_reason(name, operator, value, table) {
                        reasons.push(reason);
                    } else if let Some(reason) = table.and_then(|table| index_reason(name, table)) {
                        reasons.push(reason);
                    }
                }
                Access::Columns(a, b) => {
                    // A join key on this scan's side.
                    for column in [a, b] {
                        let (qualifier, name) = expr::split_column(column);
                        if qualifier.is_some() && qualifier == own {
                            if let Some(reason) = table.and_then(|table| index_reason(name, table)) {
                                reasons.push(reason);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let (Some(named), Some(table)) = (named, table) {
        if let Some(index) = table.indexes.iter().find(|index| index.name == named) {
            if let Some(first) = index.columns.first() {
                reasons.push(format!(
                    "{named} starts with {first}: it serves conditions on {first}"
                ));
            }
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for reason in reasons {
        if !unique.contains(&reason) {
            unique.push(reason);
        }
    }
    if unique.is_empty() {
        unique.push(match table {
            Some(table) if table.indexes.is_empty() => format!("{} has no index", table.name),
            _ => "no index matches the condition".to_owned(),
        });
    }
    unique
}

/// Whether a column belongs to the scan its qualifier names; unqualified
/// columns belong to the only relation there is.
fn belongs(column: &str, own: Option<&str>) -> bool {
    match (expr::split_column(column).0, own) {
        (Some(qualifier), Some(own)) => qualifier == own,
        _ => true,
    }
}

/// The valid, complete indexes whose first column is this one.
fn leading_names(table: &Table, column: &str) -> Vec<String> {
    table
        .indexes
        .iter()
        .filter(|index| index.columns.first().map(String::as_str) == Some(column))
        .map(|index| index.name.clone())
        .collect()
}

/// Why an operator keeps an index from serving a comparison.
fn operator_reason(
    name: &str,
    operator: &str,
    value: &str,
    table: Option<&Table>,
) -> Option<String> {
    let indexes: &[ExistingIndex] = table.map_or(&[], |table| &table.indexes);
    let leading = |index: &&ExistingIndex| index.columns.first().map(String::as_str) == Some(name);
    match operator {
        "<>" | "!=" => Some(format!(
            "{name} <> … keeps every row but some, which an index cannot narrow down"
        )),
        "~~" | "~~*" if value.starts_with("'%") || value.starts_with("'_") => Some(format!(
            "a pattern that starts with a wildcard needs a trigram index on {name} (pg_trgm, GIN)"
        )),
        "~~*" => Some(format!(
            "ILIKE on {name} needs a trigram index (pg_trgm, GIN)"
        )),
        "~~" => {
            let collation = table
                .and_then(|table| table.column(name))
                .and_then(|column| column.collation.as_deref());
            let byte_order = matches!(collation, Some("C" | "POSIX"));
            let plain = indexes
                .iter()
                .filter(leading)
                .find(|index| !index.definition.contains("_pattern_ops"));
            match (plain, collation) {
                (Some(index), Some(collation)) if !byte_order => Some(format!(
                    "LIKE 'abc%' needs the C collation or text_pattern_ops: {name} uses the {collation} collation, and {} has neither",
                    index.name
                )),
                _ => None,
            }
        }
        "@>" | "<@" | "?" | "?|" | "?&" | "&&" | "@@" => {
            let served = indexes
                .iter()
                .filter(leading)
                .any(|index| matches!(index.method.as_str(), "gin" | "gist"));
            (!served).then(|| format!("{operator} on {name} needs a GIN index"))
        }
        _ => None,
    }
}

/// Why the table's indexes do not serve a comparison of a column with a
/// value, or `None` when one should.
fn index_reason(name: &str, table: &Table) -> Option<String> {
    let leading: Vec<&ExistingIndex> = table
        .indexes
        .iter()
        .filter(|index| index.columns.first().map(String::as_str) == Some(name))
        .collect();
    if leading.iter().any(|index| index.valid && !index.partial) {
        return None;
    }
    if let Some(index) = leading.iter().find(|index| !index.valid) {
        return Some(format!(
            "{} is invalid, left by a failed CREATE INDEX CONCURRENTLY: REINDEX it",
            index.name
        ));
    }
    if let Some(index) = leading.first() {
        return Some(format!(
            "{} is a partial index, and its condition does not cover this query",
            index.name
        ));
    }
    if let Some((index, position)) = table.indexes.iter().find_map(|index| {
        index
            .columns
            .iter()
            .position(|column| column == name)
            .map(|position| (index, position))
    }) {
        return Some(format!(
            "{} has {name} as its column {}, after {}, which the condition does not narrow down",
            index.name,
            position + 1,
            index.columns[0]
        ));
    }
    Some(format!("no index on {} starts with {name}", table.name))
}

fn unusable_action(reasons: &[String]) -> String {
    if reasons
        .iter()
        .any(|reason| reason.starts_with("the condition casts"))
    {
        "Compare the column with a value of its own type, so that it stands alone in the condition (the advice shows the rewrite).".to_owned()
    } else if reasons
        .iter()
        .any(|reason| reason.starts_with("the condition applies"))
    {
        "Rewrite the condition so that the column stands alone, such as a range of values instead of a function of the column, or index the expression itself.".to_owned()
    } else if reasons.iter().any(|reason| reason.contains("is invalid")) {
        "Rebuild the invalid index (REINDEX INDEX CONCURRENTLY), or drop it and create it again."
            .to_owned()
    } else if reasons
        .iter()
        .any(|reason| reason.contains("scan has no condition"))
    {
        "Nothing for an index to do: the statement reads every row.".to_owned()
    } else {
        "Create an index that serves the condition: see the advice.".to_owned()
    }
}

fn join_answer(context: &Context, question: &Question, answer: &mut Answer) {
    let plan = context.plan;
    let join = plan.node(question.node);
    let label = format::node(join);
    let relations = fingerprint::relations(plan, join.id);
    let other = fingerprint::find_join(context.alternative, &relations);
    context.note_changes(question.node, answer);
    if other.is_some_and(|other| other.node_type == "Nested Loop") {
        answer.verdict = Verdict::Unusable;
        answer.summary = format!(
            "Only a nested loop can do this join: even with nested loops off, the planner keeps {label}. A hash or merge join needs an equality between the two sides, and {}.",
            match join.predicate(PredicateKind::JoinFilter) {
                Some(condition) => format!("the join condition is {condition}"),
                None => "this join has no condition between them".to_owned(),
            }
        );
        answer.action = Some(
            "Make the join an equality of columns, if the query allows it; otherwise keep the inner side cheap with an index on what it compares.".to_owned(),
        );
        return;
    }
    let path = other.map_or_else(
        || "a plan without nested loops".to_owned(),
        |other| format!("a {}", format::node(other)),
    );
    let comparison = context.comparison(answer.measured);
    answer
        .evidence
        .push(evidence("Planner's choice", plan_cost(&label, plan)));
    answer.evidence.push(evidence(
        "Alternative",
        plan_cost(path.trim_start_matches("a "), context.alternative),
    ));
    if !answer.measured {
        answer.verdict = Verdict::Costlier;
        let estimate = cost_comparison(
            compare::planner_cost(plan),
            compare::planner_cost(context.alternative),
        );
        answer.summary = match estimate {
            Some((increase, text)) if increase < CLOSE_CALL => format!(
                "With {path} the statement is about as expensive by the planner's estimate ({text}). A small change in the statistics can flip this plan."
            ),
            Some((_, text)) => format!(
                "With {path} the planner estimates the statement {text}, so it chose the nested loop."
            ),
            None => format!("The planner estimates {path} more expensive."),
        };
        answer.action = Some(
            "Measure both plans to know whether the planner is right: --measure, or y in a viewer started with --measure.".to_owned(),
        );
        answer.comparison = Some(comparison);
        return;
    }
    // A nested loop is chosen when the outer side looks small.
    let underestimate = plan
        .children(join.id)
        .filter(|child| {
            !matches!(
                child.relationship,
                Some(Relationship::InitPlan | Relationship::SubPlan)
            )
        })
        .find_map(|child| {
            context
                .misestimate(Found::Node(child.id))
                .filter(|(_, error)| error.underestimated && error.factor >= MISESTIMATE)
        });
    match comparison.change {
        Change::Better => {
            if let Some((node, error)) = underestimate {
                answer.verdict = Verdict::Misestimate;
                let rows = rows_line(node, error);
                answer.summary = format!(
                    "With {path} the statement is better ({}). The planner chose the nested loop because it expected {}.",
                    comparison.details(),
                    rows
                );
                answer.evidence.push(evidence("Row estimate", rows));
                answer.action = Some(format!(
                    "Fix the estimate of {}: see ES002 for how. With the real row count, the planner weighs the join methods correctly.",
                    format::node(node)
                ));
            } else {
                answer.verdict = Verdict::PlannerWrong;
                answer.summary = format!(
                    "With {path} the statement is better ({}), though the planner did not expect too few rows from the join's inputs.",
                    comparison.details()
                );
                answer.action = Some(
                    "Check the cost settings (random_page_cost, effective_cache_size). Until then, a hint (pg_hint_plan) or, from PostgreSQL 19, pg_plan_advice can keep the better plan.".to_owned(),
                );
            }
        }
        Change::Worse | Change::Same => {
            answer.verdict = Verdict::PlannerRight;
            answer.summary = format!(
                "The planner is right: with {path} the statement is {} ({}).",
                comparison.describe(),
                comparison.details()
            );
            answer.action =
                Some("Nothing to change: the nested loop is the better join here.".to_owned());
        }
        Change::Mixed | Change::Unknown => inconclusive(&comparison, answer),
    }
    answer.comparison = Some(comparison);
}

fn memory_answer(context: &Context, question: &Question, work_mem: &str, answer: &mut Answer) {
    let label = format::node(context.plan.node(question.node));
    if !answer.measured {
        answer.summary = format!(
            "Whether {label} stays in memory shows only in a measured run: measure with --measure."
        );
        return;
    }
    context.note_changes(question.node, answer);
    let comparison = context.comparison(true);
    let still_spills = comparison.after.temp_pages.is_some_and(|pages| pages > 0);
    // Memory is for speed: a statement that stays in memory but runs
    // slower is not helped, whatever it no longer writes.
    if comparison.time_change() == Some(Change::Worse) {
        answer.verdict = Verdict::MoreMemoryDoesNotHelp;
        answer.summary = format!(
            "With work_mem = {work_mem}, {label} {}, but the statement is slower ({}): here the operation is faster with what it writes to disk.",
            if still_spills {
                "spills less"
            } else {
                "stays in memory"
            },
            comparison.details()
        );
        answer.action = Some("Leave work_mem as it is.".to_owned());
        answer.comparison = Some(comparison);
        return;
    }
    match comparison.change {
        Change::Better => {
            answer.verdict = Verdict::MoreMemoryHelps;
            answer.summary = format!(
                "With work_mem = {work_mem}, {label} {} and the statement is better ({}).",
                if still_spills {
                    "spills less"
                } else {
                    "stays in memory"
                },
                comparison.details()
            );
            answer.action = Some(format!(
                "Give this statement work_mem = {work_mem}: SET LOCAL work_mem = '{work_mem}' in its transaction, or ALTER ROLE … SET work_mem for the role that runs it. For every connection it would let each sort and hash of every session use that much at once."
            ));
        }
        Change::Worse | Change::Same => {
            answer.verdict = Verdict::MoreMemoryDoesNotHelp;
            answer.summary = format!(
                "With work_mem = {work_mem} the statement is {} ({}).",
                comparison.describe(),
                comparison.details()
            );
            answer.action =
                Some("Leave work_mem as it is: the spill costs little here.".to_owned());
        }
        Change::Mixed | Change::Unknown => inconclusive(&comparison, answer),
    }
    answer.comparison = Some(comparison);
}

/// Puts what the database said into the advice about the same scans: an
/// existing index the planner did not use gets the reason found, instead of
/// the likely ones.
pub fn annotate(advice: &mut [Advice], answers: &[Answer]) {
    for item in advice.iter_mut() {
        if !matches!(item.kind, AdviceKind::AlreadyIndexed { .. }) {
            continue;
        }
        let Some(answer) = answers.iter().find(|answer| {
            Some(answer.node) == item.node && matches!(answer.topic, Topic::Index { .. })
        }) else {
            continue;
        };
        // Keep "X already serves …, but the planner did not use it." and
        // replace the guesses after it.
        let first = item
            .summary
            .split_once(". ")
            .map_or(item.summary.as_str(), |(first, _)| first)
            .trim_end_matches('.')
            .to_owned();
        item.summary = format!("{first}. {}", answer.summary);
        item.evidence
            .retain(|evidence| evidence.label != "Asked the database");
        item.evidence
            .push(evidence("Asked the database", answer.verdict.describe()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Column;

    fn parse(text: &str) -> Plan {
        let plan = crate::parse(text).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        plan
    }

    /// A sequential scan that keeps 10 rows of 200,000, measured.
    fn seq_scan(estimated_rows: u64, pages: u64, ms: &str) -> Plan {
        parse(&format!(
            "\
Seq Scan on orders  (cost=0.00..4917.00 rows={estimated_rows} width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit={pages}
Execution Time: {ms} ms"
        ))
    }

    fn index_scan(pages: u64, ms: &str) -> Plan {
        parse(&format!(
            "\
Index Scan using orders_customer_id_idx on orders  (cost=0.42..5210.00 rows=10 width=64) (actual time=0.020..0.031 rows=10 loops=1)
  Index Cond: (customer_id = 4242)
  Buffers: shared hit={pages}
Execution Time: {ms} ms"
        ))
    }

    const ESTIMATED_INDEX_SCAN: &str = "\
Bitmap Heap Scan on orders  (cost=12.00..5210.00 rows=10 width=64)
  Recheck Cond: (customer_id = 4242)
  ->  Bitmap Index Scan on orders_customer_id_idx  (cost=0.00..12.00 rows=10 width=0)
        Index Cond: (customer_id = 4242)";

    fn ask(plan: &Plan, target: &Target, measure: bool) -> Vec<Question> {
        questions(plan, &crate::analyze(plan), None, target, measure)
    }

    fn orders(indexes: Vec<ExistingIndex>) -> Catalog {
        Catalog {
            tables: vec![Table {
                schema: "public".to_owned(),
                name: "orders".to_owned(),
                indexes,
                columns: vec![
                    Column {
                        name: "customer_id".to_owned(),
                        type_name: Some("integer".to_owned()),
                        ..Column::default()
                    },
                    Column {
                        name: "note".to_owned(),
                        type_name: Some("text".to_owned()),
                        collation: Some("en_US.utf8".to_owned()),
                        ..Column::default()
                    },
                ],
                ..Table::default()
            }],
            ..Catalog::default()
        }
    }

    fn btree(name: &str, columns: &[&str]) -> ExistingIndex {
        ExistingIndex {
            name: name.to_owned(),
            definition: format!(
                "CREATE INDEX {name} ON public.orders USING btree ({})",
                columns.join(", ")
            ),
            method: "btree".to_owned(),
            columns: columns.iter().map(|column| (*column).to_owned()).collect(),
            valid: true,
            ..ExistingIndex::default()
        }
    }

    fn respond(
        plan: &Plan,
        question: &Question,
        chosen: &[Plan],
        alternative: &[Plan],
        with_cost_settings: Option<&Plan>,
        catalog: Option<&Catalog>,
    ) -> Answer {
        respond_measuring_costs(
            plan,
            question,
            chosen,
            alternative,
            with_cost_settings,
            &[],
            catalog,
        )
    }

    fn respond_measuring_costs(
        plan: &Plan,
        question: &Question,
        chosen: &[Plan],
        alternative: &[Plan],
        with_cost_settings: Option<&Plan>,
        cost_settings_runs: &[Plan],
        catalog: Option<&Catalog>,
    ) -> Answer {
        answer(
            plan,
            &crate::analyze(plan),
            question,
            &Evaluation {
                chosen,
                alternative,
                with_cost_settings,
                cost_settings_runs,
                catalog,
            },
        )
    }

    #[test]
    fn asks_about_the_hot_scans() {
        let plan = seq_scan(10, 2417, "11.900");
        let asked = ask(&plan, &Target::Hotspots, false);
        assert_eq!(asked.len(), 1);
        let question = &asked[0];
        assert_eq!(
            question.text,
            "Why does Seq Scan on orders not use an index?"
        );
        assert_eq!(question.settings, [Setting::new("enable_seqscan", "off")]);
        assert_eq!(
            question.cost_settings,
            [Setting::new("random_page_cost", "1.1")]
        );
        // Every setting asked for is one explainsql applies.
        assert!(
            question
                .settings
                .iter()
                .all(|setting| setting.check().is_ok())
        );
        // Named by its table, or by an index the catalog knows.
        assert_eq!(ask(&plan, &Target::parse("public.orders"), false).len(), 1);
        let catalog = orders(vec![btree("orders_customer_id_idx", &["customer_id"])]);
        let named = questions(
            &plan,
            &crate::analyze(&plan),
            Some(&catalog),
            &Target::parse("orders_customer_id_idx"),
            false,
        );
        assert_eq!(
            named[0].text,
            "Why does Seq Scan on orders not use orders_customer_id_idx?"
        );
        // Nothing to ask about a scan without a condition, unless named.
        let all = parse(
            "Seq Scan on orders  (cost=0.00..4917.00 rows=200000 width=64) (actual time=0.010..20.000 rows=200000 loops=1)\nExecution Time: 25.000 ms",
        );
        assert!(ask(&all, &Target::Hotspots, false).is_empty());
        assert_eq!(ask(&all, &Target::Node(NodeId(0)), false).len(), 1);
        // No cost settings to try when random_page_cost is low already.
        let tuned = parse(
            "Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)\n  Filter: (customer_id = 4242)\nSettings: random_page_cost = '1.1'\nExecution Time: 11.900 ms",
        );
        assert!(
            ask(&tuned, &Target::Hotspots, false)[0]
                .cost_settings
                .is_empty()
        );
    }

    #[test]
    fn an_index_that_cannot_serve_the_condition() {
        let plan = parse(
            "\
Seq Scan on orders  (cost=0.00..5417.00 rows=1000 width=64) (actual time=0.030..30.000 rows=274 loops=1)
  Filter: (date_trunc('day'::text, created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)
  Rows Removed by Filter: 199726
  Buffers: shared hit=2417
Execution Time: 30.100 ms",
        );
        let question = &ask(&plan, &Target::Hotspots, false)[0];
        // With sequential scans off, the planner reads the whole index.
        let whole = parse(
            "\
Index Scan using orders_created_at_idx on orders  (cost=0.42..12000.00 rows=1000 width=64)
  Filter: (date_trunc('day'::text, created_at) = '2024-06-01 00:00:00+00'::timestamp with time zone)",
        );
        let catalog = orders(vec![
            btree("orders_pkey", &["id"]),
            btree("orders_created_at_idx", &["created_at"]),
        ]);
        let answer = respond(&plan, question, &[], &[whole], None, Some(&catalog));
        assert_eq!(answer.verdict, Verdict::Unusable);
        assert!(
            answer
                .summary
                .contains("reads all of orders_created_at_idx instead"),
            "{}",
            answer.summary
        );
        assert_eq!(
            answer.evidence[0].value,
            "the condition applies date_trunc() to created_at, so orders_created_at_idx cannot serve it"
        );
        assert!(
            answer
                .action
                .as_deref()
                .unwrap()
                .starts_with("Rewrite the condition")
        );

        // Before PostgreSQL 18: the scan stays, disabled.
        let cast = parse(
            "Seq Scan on orders  (cost=0.00..5417.00 rows=1000 width=64) (actual time=0.030..30.000 rows=10 loops=1)\n  Filter: ((customer_id)::text = '4242'::text)\n  Rows Removed by Filter: 199990\nExecution Time: 30.100 ms",
        );
        let question = &ask(&cast, &Target::Hotspots, false)[0];
        let disabled = parse(
            "Seq Scan on orders  (cost=10000000000.00..10000005417.00 rows=1000 width=64)\n  Filter: ((customer_id)::text = '4242'::text)",
        );
        let answer = respond(&cast, question, &[], &[disabled], None, Some(&catalog));
        assert_eq!(answer.verdict, Verdict::Unusable);
        assert_eq!(
            answer.evidence[0].value,
            "the condition casts customer_id (integer), so an index on customer_id cannot serve it"
        );
    }

    #[test]
    fn names_what_keeps_an_index_out() {
        let scan = |filter: &str| {
            parse(&format!(
                "Seq Scan on orders  (cost=0.00..5417.00 rows=10 width=64) (actual time=0.030..30.000 rows=10 loops=1)\n  Filter: {filter}\n  Rows Removed by Filter: 199990\nExecution Time: 30.100 ms"
            ))
        };
        let reasons = |filter: &str, catalog: &Catalog| {
            let plan = scan(filter);
            let disabled = parse(&format!(
                "Seq Scan on orders  (cost=10000000000.00..10000005417.00 rows=10 width=64)\n  Filter: {filter}"
            ));
            let question = &ask(&plan, &Target::Hotspots, false)[0];
            let answer = respond(&plan, question, &[], &[disabled], None, Some(catalog));
            assert_eq!(answer.verdict, Verdict::Unusable);
            answer
                .evidence
                .iter()
                .filter(|evidence| evidence.label == "Why")
                .map(|evidence| evidence.value.clone())
                .collect::<Vec<_>>()
        };
        let catalog = orders(vec![btree(
            "orders_status_customer_idx",
            &["status", "customer_id"],
        )]);
        assert_eq!(
            reasons("(customer_id = 4242)", &catalog),
            [
                "orders_status_customer_idx has customer_id as its column 2, after status, which the condition does not narrow down"
            ]
        );
        assert_eq!(
            reasons("(note ~~ '%abc%'::text)", &catalog),
            ["a pattern that starts with a wildcard needs a trigram index on note (pg_trgm, GIN)"]
        );
        let catalog = orders(vec![btree("orders_note_idx", &["note"])]);
        assert_eq!(
            reasons("(note ~~ 'abc%'::text)", &catalog),
            [
                "LIKE 'abc%' needs the C collation or text_pattern_ops: note uses the en_US.utf8 collation, and orders_note_idx has neither"
            ]
        );
        let mut invalid = btree("orders_customer_id_idx", &["customer_id"]);
        invalid.valid = false;
        assert_eq!(
            reasons("(customer_id = 4242)", &orders(vec![invalid])),
            [
                "orders_customer_id_idx is invalid, left by a failed CREATE INDEX CONCURRENTLY: REINDEX it"
            ]
        );
        assert_eq!(
            reasons(
                "((customer_id = 4242) OR (status = 'x'::text))",
                &orders(Vec::new())
            ),
            [
                "the filter ORs conditions on different columns, which no single index serves; an index on each column lets PostgreSQL combine them with a BitmapOr"
            ]
        );
        assert_eq!(
            reasons("(customer_id = 4242)", &orders(Vec::new())),
            ["no index on orders starts with customer_id"]
        );
    }

    #[test]
    fn estimates_say_how_much_the_planner_preferred_its_plan() {
        let plan = seq_scan(10, 2417, "11.900");
        let question = &ask(&plan, &Target::Hotspots, false)[0];
        let alternative = parse(ESTIMATED_INDEX_SCAN);
        let cheap = parse(
            "Index Scan using orders_customer_id_idx on orders  (cost=0.42..40.00 rows=10 width=64)\n  Index Cond: (customer_id = 4242)",
        );
        let answer = respond(&plan, question, &[], &[alternative], Some(&cheap), None);
        assert_eq!(answer.verdict, Verdict::Costlier);
        assert!(!answer.measured);
        assert_eq!(
            answer.summary,
            "PostgreSQL can use Bitmap Heap Scan on orders, and estimates it about as expensive as the sequential scan (a close call: 5,210 against 4,917, within 10%). A small change in the statistics or the cost settings can flip this plan."
        );
        assert!(answer.evidence.iter().any(|evidence| evidence.label
            == "With random_page_cost = 1.1"
            && evidence.value
                == "the planner chooses Index Scan using orders_customer_id_idx on orders by itself"));
        assert!(answer.action.as_deref().unwrap().contains("--measure"));
    }

    #[test]
    fn measurements_say_whether_the_planner_is_right() {
        let plan = seq_scan(10, 2417, "11.900");
        let question = &ask(&plan, &Target::Hotspots, true)[0];
        let chosen = [seq_scan(10, 2417, "11.900")];
        let keeps = parse(
            "Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64)\n  Filter: (customer_id = 4242)",
        );
        let picks = parse(
            "Index Scan using orders_customer_id_idx on orders  (cost=0.42..40.00 rows=10 width=64)\n  Index Cond: (customer_id = 4242)",
        );

        // Fewer pages with the index, the estimate was right, and with
        // random_page_cost = 1.1 the planner picks a plan that measures
        // better too: the cost settings.
        let better = [index_scan(13, "0.050")];
        assert!(measure_cost_settings(question, &picks));
        assert!(!measure_cost_settings(question, &keeps));
        let answer = respond_measuring_costs(
            &plan,
            question,
            &chosen,
            &better,
            Some(&picks),
            &better,
            None,
        );
        assert_eq!(answer.verdict, Verdict::CostSettings, "{}", answer.summary);
        assert!(answer.measured);
        assert!(
            answer
                .summary
                .contains("the planner chooses it by itself, and that is better as well"),
            "{}",
            answer.summary
        );
        assert!(
            answer.summary.contains("four times as expensive"),
            "{}",
            answer.summary
        );
        // The plan random_page_cost = 1.1 leads to is no better: no
        // setting to suggest.
        let answer = respond_measuring_costs(
            &plan,
            question,
            &chosen,
            &better,
            Some(&picks),
            &[index_scan(2500, "12.000")],
            None,
        );
        assert_eq!(answer.verdict, Verdict::PlannerWrong, "{}", answer.summary);
        assert!(
            answer
                .summary
                .contains("lowering random_page_cost alone does not fix this plan"),
            "{}",
            answer.summary
        );
        // Not measured with the cost settings, or they keep the scan:
        // wrong, for a reason not found.
        let answer = respond(&plan, question, &chosen, &better, Some(&picks), None);
        assert_eq!(answer.verdict, Verdict::PlannerWrong);
        let answer = respond(&plan, question, &chosen, &better, Some(&keeps), None);
        assert_eq!(answer.verdict, Verdict::PlannerWrong);

        // The planner expected 50,000 rows where 10 came.
        let misled = [seq_scan(50_000, 2417, "11.900")];
        let answer = respond(&plan, question, &misled, &better, Some(&keeps), None);
        assert_eq!(answer.verdict, Verdict::Misestimate);
        assert!(answer.summary.ends_with("because it expected 50,000 rows from Seq Scan on orders where 10 came (5,000× fewer)."), "{}", answer.summary);

        // More pages with the index: the planner is right.
        let worse = [index_scan(3000, "14.000")];
        let answer = respond(&plan, question, &chosen, &worse, Some(&keeps), None);
        assert_eq!(answer.verdict, Verdict::PlannerRight);
        assert_eq!(
            answer.summary,
            "The planner is right: with Index Scan using orders_customer_id_idx on orders the statement is worse (pages 2,417 → 3,000 (1.2× more), execution 11.9 ms → 14.0 ms (1.2× slower))."
        );
        // Fewer pages but much slower: not clear.
        let mixed = [index_scan(13, "40.000")];
        let answer = respond(&plan, question, &chosen, &mixed, None, None);
        assert_eq!(answer.verdict, Verdict::Inconclusive);
    }

    #[test]
    fn says_when_the_settings_changed_more_than_the_node() {
        let plan = parse(
            "\
Hash Join  (cost=10.00..6000.00 rows=10 width=72) (actual time=5.000..20.000 rows=10 loops=1)
  Hash Cond: (o.customer_id = c.id)
  Buffers: shared hit=2500
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=64) (actual time=0.010..15.000 rows=10 loops=1)
        Filter: (note = 'x'::text)
        Rows Removed by Filter: 199990
        Buffers: shared hit=2417
  ->  Hash  (cost=5.00..5.00 rows=50 width=8) (actual time=1.000..1.000 rows=50 loops=1)
        Buffers: shared hit=83
        ->  Seq Scan on customers c  (cost=0.00..5.00 rows=50 width=8) (actual time=0.010..0.500 rows=50 loops=1)
              Buffers: shared hit=83
Execution Time: 20.500 ms",
        );
        let question = &ask(&plan, &Target::Node(NodeId(1)), false)[0];
        let alternative = parse(
            "\
Nested Loop  (cost=0.71..7000.00 rows=10 width=72)
  ->  Index Scan using orders_note_idx on orders o  (cost=0.42..6000.00 rows=10 width=64)
        Index Cond: (note = 'x'::text)
  ->  Index Scan using customers_pkey on customers c  (cost=0.29..8.30 rows=1 width=8)
        Index Cond: (id = o.customer_id)",
        );
        let answer = respond(&plan, question, &[], &[alternative], None, None);
        assert_eq!(answer.verdict, Verdict::Costlier);
        assert!(answer.approximate);
        let changed: Vec<&str> = answer
            .evidence
            .iter()
            .filter(|evidence| evidence.label == "Also changed")
            .map(|evidence| evidence.value.as_str())
            .collect();
        assert_eq!(
            changed,
            [
                "Nested Loop (was Hash Join); Index Scan using customers_pkey on customers c (was Seq Scan on customers c)"
            ]
        );
    }

    const NESTED_LOOP: &str = "\
Nested Loop  (cost=0.00..9000.00 rows=1 width=8) (actual time=0.100..900.000 rows=500 loops=1)
  Join Filter: (oi.order_id = o.id)
  Rows Removed by Join Filter: 149999500
  Buffers: shared hit=955500
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=1 width=4) (actual time=0.010..20.000 rows=500 loops=1)
        Filter: (customer_id = 4242)
        Rows Removed by Filter: 199500
        Buffers: shared hit=2417
  ->  Seq Scan on order_items oi  (cost=0.00..4911.00 rows=300000 width=8) (actual time=0.001..1.700 rows=300000 loops=500)
        Buffers: shared hit=953083
Execution Time: 900.500 ms";

    #[test]
    fn asks_why_a_nested_loop() {
        let plan = parse(NESTED_LOOP);
        let questions = ask(&plan, &Target::Hotspots, true);
        let question = questions
            .iter()
            .find(|question| question.topic == Topic::NestedLoop)
            .unwrap();
        assert_eq!(question.settings, [Setting::new("enable_nestloop", "off")]);
        assert_eq!(
            question.text,
            "Why does the join of order_items oi and orders o run as a nested loop rather than a hash or merge join?"
        );
        let hash = parse(
            "\
Hash Join  (cost=5000.00..12000.00 rows=1 width=8) (actual time=30.000..60.000 rows=500 loops=1)
  Hash Cond: (oi.order_id = o.id)
  Buffers: shared hit=4328
  ->  Seq Scan on order_items oi  (cost=0.00..4911.00 rows=300000 width=8) (actual time=0.010..20.000 rows=300000 loops=1)
        Buffers: shared hit=1911
  ->  Hash  (cost=4917.00..4917.00 rows=1 width=4) (actual time=20.000..20.000 rows=500 loops=1)
        Buffers: shared hit=2417
        ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=1 width=4) (actual time=0.010..20.000 rows=500 loops=1)
              Filter: (customer_id = 4242)
              Rows Removed by Filter: 199500
              Buffers: shared hit=2417
Execution Time: 60.500 ms",
        );
        let answer = respond(
            &plan,
            question,
            std::slice::from_ref(&plan),
            &[hash],
            None,
            None,
        );
        assert_eq!(answer.verdict, Verdict::Misestimate, "{}", answer.summary);
        assert!(answer.summary.contains("a Hash Join"), "{}", answer.summary);
        assert!(
            answer
                .summary
                .ends_with("expected 1 row from Seq Scan on orders o where 500 came (500× more)."),
            "{}",
            answer.summary
        );

        // A join whose condition is not an equality stays a nested loop.
        let still = parse(
            "\
Nested Loop  (cost=10000000000.00..10000009000.00 rows=1 width=8)
  Join Filter: (oi.order_id = o.id)
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=1 width=4)
        Filter: (customer_id = 4242)
  ->  Seq Scan on order_items oi  (cost=0.00..4911.00 rows=300000 width=8)",
        );
        let answer = respond(&plan, question, &[], &[still], None, None);
        assert_eq!(answer.verdict, Verdict::Unusable);
    }

    #[test]
    fn asks_whether_more_memory_helps() {
        let sort = |space: &str, temp: &str, ms: &str| {
            parse(&format!(
                "\
Sort  (cost=30000.00..30500.00 rows=200000 width=40) (actual time=150.000..180.000 rows=200000 loops=1)
  Sort Key: note
  Sort Method: {space}
  Buffers: shared hit=2417{temp}
  ->  Seq Scan on orders  (cost=0.00..4417.00 rows=200000 width=40) (actual time=0.010..20.000 rows=200000 loops=1)
        Buffers: shared hit=2417
Execution Time: {ms} ms"
            ))
        };
        let plan = sort(
            "external merge  Disk: 10000kB",
            ", temp read=1250 written=1253",
            "190.000",
        );
        assert!(ask(&plan, &Target::Hotspots, false).is_empty());
        let questions = ask(&plan, &Target::Hotspots, true);
        assert_eq!(questions.len(), 1);
        let question = &questions[0];
        // About three times the room the sort took on disk.
        assert_eq!(
            question.topic,
            Topic::Memory {
                work_mem: "32MB".to_owned()
            }
        );
        assert_eq!(question.settings, [Setting::new("work_mem", "32MB")]);
        let in_memory = sort("quicksort  Memory: 25000kB", "", "120.000");
        let answer = respond(
            &plan,
            question,
            std::slice::from_ref(&plan),
            &[in_memory],
            None,
            None,
        );
        // In memory but slower: an external merge can beat a quicksort.
        let slower = sort("quicksort  Memory: 25000kB", "", "260.000");
        let answer_slower = respond(
            &plan,
            question,
            std::slice::from_ref(&plan),
            &[slower],
            None,
            None,
        );
        assert_eq!(answer_slower.verdict, Verdict::MoreMemoryDoesNotHelp);
        assert!(
            answer_slower
                .summary
                .contains("stays in memory, but the statement is slower"),
            "{}",
            answer_slower.summary
        );
        assert_eq!(answer.verdict, Verdict::MoreMemoryHelps);
        assert_eq!(
            answer.summary,
            "With work_mem = 32MB, Sort stays in memory and the statement is better (pages 2,417 → 2,417, temporary files 2,503 → 0 pages, execution 190.0 ms → 120.0 ms (1.6× faster))."
        );
    }

    #[test]
    fn puts_the_reason_into_the_advice() {
        let plan = seq_scan(10, 2417, "11.900");
        let mut advice = vec![Advice {
            node: Some(NodeId(0)),
            kind: AdviceKind::AlreadyIndexed {
                index: "orders_customer_id_idx".to_owned(),
                definition: String::new(),
            },
            confidence: crate::advisor::Confidence::High,
            summary: "orders_customer_id_idx already serves orders (customer_id), but the planner did not use it here. The likely reasons: …".to_owned(),
            evidence: Vec::new(),
            caveats: Vec::new(),
            verification: crate::advisor::Verification::Unverified,
            proof: None,
            rules: Vec::new(),
        }];
        let question = &ask(&plan, &Target::Hotspots, true)[0];
        let answer = respond(
            &plan,
            question,
            &[seq_scan(50_000, 2417, "11.900")],
            &[index_scan(13, "0.050")],
            None,
            None,
        );
        annotate(&mut advice, &[answer]);
        assert!(
            advice[0].summary.starts_with("orders_customer_id_idx already serves orders (customer_id), but the planner did not use it here. With Index Scan"),
            "{}",
            advice[0].summary
        );
        assert_eq!(
            advice[0].evidence.last().unwrap().value,
            "the planner chose wrongly because of a row misestimate"
        );
    }
}
