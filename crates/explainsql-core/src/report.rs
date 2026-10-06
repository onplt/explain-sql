//! Static reports of an analysis: text for terminals, Markdown for issues
//! and pull requests, JSON for other programs.

use serde::Serialize;

use crate::advisor::{Advice, AdviceKind, Confidence, Verification};
use crate::analysis::Analysis;
use crate::check::{self, Checked, Status};
use crate::counterfactual::{Answer, Verdict};
use crate::diff::{ChangeKind, PlanDiff};
use crate::format;
use crate::ir::{NodeId, Plan};
use crate::metrics;
use crate::params::{self, Parameter, Sensitivity};
use crate::rules::{Finding, Severity};

/// Node labels longer than this are shortened in the plan table.
const MAX_NODE_WIDTH: usize = 64;
/// Where text is wrapped.
const LINE_WIDTH: usize = 100;
const BAR_WIDTH: usize = 10;

/// I/O from this share of the time is among the statement's facts.
const MENTIONED_IO: f64 = 0.1;

/// Misestimates from this factor are marked in the plan table.
const MARKED_MISESTIMATE: f64 = 10.0;

/// The report for a terminal. `color` adds ANSI colors.
pub fn text(plan: &Plan, analysis: &Analysis, color: bool) -> String {
    let paint = Paint(color);
    let mut out = String::new();
    if let Some(sensitivity) = &analysis.parameters {
        text_parameters(&mut out, sensitivity, &paint);
        out.push('\n');
    }
    out.push_str(&paint.bold(&wrap(&analysis.verdict, 0)));
    out.push_str("\n\n");
    let facts = facts(plan, analysis);
    if !facts.is_empty() {
        out.push_str(&paint.dim(&wrap(&facts.join(" · "), 0)));
        out.push_str("\n\n");
    }

    write_table(&mut out, &table_rows(plan, analysis), &paint, |_| "");
    out.push('\n');

    if analysis.findings.is_empty() {
        out.push_str("No findings: none of the rules found a problem in this plan.\n");
    } else {
        out.push_str(&paint.bold(&format!("Findings ({})", analysis.findings.len())));
        out.push('\n');
        for finding in &analysis.findings {
            out.push('\n');
            out.push_str(&format!(
                "  {}  {} {}\n",
                paint.severity(finding.severity),
                finding.rule.id,
                finding.rule.name
            ));
            out.push_str(&wrap(&format!("{}.", finding.summary), 10));
            out.push('\n');
            for evidence in &finding.evidence {
                out.push_str(&wrap(
                    &format!("{}: {}", evidence.label, evidence.value),
                    10,
                ));
                out.push('\n');
            }
            out.push_str(&wrap(&format!("→ {}", finding.action), 10));
            out.push('\n');
        }
    }

    text_advice(&mut out, plan, analysis, &paint);
    text_counterfactuals(&mut out, analysis, &paint);

    if !plan.warnings.is_empty() {
        out.push('\n');
        out.push_str(&paint.bold("Parser warnings"));
        out.push('\n');
        for warning in &plan.warnings {
            let text = match warning.line {
                Some(line) => format!("line {line}: {}", warning.message),
                None => warning.message.clone(),
            };
            out.push_str(&wrap(&text, 2));
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str(&paint.dim(&wrap(LEGEND, 0)));
    out.push('\n');
    out
}

const LEGEND: &str = "Share and Time: time spent in each node itself, wall-clock. Rows: actual rows per loop, ×loops when it ran more than once. ▲/▼: actual rows above or below the estimate by 10× or more. Buffers: pages each node read itself.";

/// How many nodes a diff report names before summing up the rest.
const MAX_LISTED: usize = 12;

/// A plan diff for a terminal: the verdict, the changes, then the second
/// plan with its changed (`~`) and new (`+`) nodes marked. `color` adds
/// ANSI colors.
pub fn diff_text(before: &Plan, after: &Plan, diff: &PlanDiff, color: bool) -> String {
    let paint = Paint(color);
    let mut out = String::new();
    out.push_str(&paint.bold(&wrap(&diff.verdict, 0)));
    out.push_str("\n\n");
    out.push_str(&paint.dim(&shapes_line(diff)));
    out.push_str("\n\n");
    if diff.changes.is_empty() {
        out.push_str("No changes: the nodes of both plans did the same work, within the noise.\n");
    } else {
        out.push_str(&paint.bold(&format!("Changes ({})", diff.changes.len())));
        out.push('\n');
        for change in &diff.changes {
            out.push('\n');
            out.push_str(&format!("  {}  ", paint.change(change.kind)));
            out.push_str(wrap(&change.summary, 12).trim_start());
            out.push('\n');
            for evidence in &change.evidence {
                out.push_str(&wrap(
                    &format!("{}: {}", evidence.label, evidence.value),
                    12,
                ));
                out.push('\n');
            }
        }
    }
    out.push('\n');
    out.push_str(&paint.bold("The plan after"));
    out.push_str("\n\n");
    let analysis = crate::analyze(after);
    write_table(&mut out, &table_rows(after, &analysis), &paint, |id| {
        diff_marker(diff, id)
    });
    let removed = removed_labels(before, diff);
    if !removed.is_empty() {
        out.push('\n');
        out.push_str(&wrap(
            &format!("Only in the plan before: {}.", removed.join("; ")),
            0,
        ));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&paint.dim(&wrap(
        &format!("~: a node that changed. +: a node only the plan after has. {LEGEND}"),
        0,
    )));
    out.push('\n');
    out
}

/// A plan diff as Markdown, for an issue or a pull request.
pub fn diff_markdown(before: &Plan, after: &Plan, diff: &PlanDiff) -> String {
    let mut out = format!("**{}**\n\n", escape(&diff.verdict));
    out.push_str(&format!("{}\n\n", escape(&shapes_line(diff))));
    if diff.changes.is_empty() {
        out.push_str("No changes: the nodes of both plans did the same work, within the noise.\n");
    } else {
        out.push_str("### Changes\n\n");
        for change in &diff.changes {
            out.push_str(&format!(
                "- **{}:** {}.\n",
                change_name(change.kind),
                escape(&change.summary)
            ));
            for evidence in &change.evidence {
                out.push_str(&format!(
                    "  - {}: `{}`\n",
                    evidence.label,
                    evidence.value.replace('`', "'")
                ));
            }
        }
    }
    let analysis = crate::analyze(after);
    out.push_str("\n### The plan after\n\n");
    out.push_str("| | Share | Time | Node | Rows | Estimate | Buffers |\n");
    out.push_str("|---|---:|---:|---|---:|---:|---:|\n");
    for (row, (depth, _)) in table_rows(after, &analysis).iter().zip(after.walk()) {
        let indent = "&nbsp;&nbsp;".repeat(depth);
        let arrow = if depth > 0 { "↳ " } else { "" };
        out.push_str(&format!(
            "| {} | {} | {} | {indent}{arrow}{} | {} | {} | {} |\n",
            diff_marker(diff, row.id).trim(),
            row.share,
            row.time,
            escape(&row.label),
            row.rows,
            row.estimate,
            row.buffers
        ));
    }
    let removed = removed_labels(before, diff);
    if !removed.is_empty() {
        out.push_str(&format!(
            "\nOnly in the plan before: {}.\n",
            escape(&removed.join("; "))
        ));
    }
    out.push_str("\n`~` a node that changed, `+` a node only the plan after has.\n");
    out
}

/// A plan diff as JSON: the diff, with the labels of the nodes it names.
pub fn diff_json(before: &Plan, after: &Plan, diff: &PlanDiff) -> String {
    #[derive(Serialize)]
    struct Report<'a> {
        #[serde(flatten)]
        diff: &'a PlanDiff,
        /// The label of each node of each plan, by id.
        labels: Labels,
    }
    #[derive(Serialize)]
    struct Labels {
        before: Vec<String>,
        after: Vec<String>,
    }
    let report = Report {
        diff,
        labels: Labels {
            before: before.nodes.iter().map(format::node).collect(),
            after: after.nodes.iter().map(format::node).collect(),
        },
    };
    let mut out = serde_json::to_string_pretty(&report).expect("the report serializes");
    out.push('\n');
    out
}

/// The checks of a CI run for a terminal: each plan's result, why it
/// failed, what to do about it, and a summary. `color` adds ANSI colors.
pub fn check_text(checked: &[Checked], color: bool) -> String {
    let paint = Paint(color);
    let mut out = String::new();
    for item in checked {
        out.push_str(&format!(
            "{}  {}",
            paint.status(item.status),
            paint.bold(&item.name)
        ));
        out.push_str(&paint.dim(&format!("  {}", shape_note(item))));
        out.push('\n');
        for reason in &item.reasons {
            out.push_str(&wrap(reason, 6));
            out.push('\n');
        }
        for note in &item.notes {
            out.push_str(&wrap(&format!("Note: {note}"), 6));
            out.push('\n');
        }
        if item.status == Status::Failed {
            for fix in fixes(item) {
                out.push_str(&wrap(&format!("→ {fix}"), 6));
                out.push('\n');
            }
        }
    }
    if !checked.is_empty() {
        out.push('\n');
    }
    out.push_str(&paint.bold(&check::summary(checked)));
    out.push('\n');
    out
}

/// The checks of a CI run as Markdown, for a pull request comment: a table
/// of the plans, then what changed in each plan that failed or changed.
pub fn check_markdown(checked: &[Checked]) -> String {
    let failed = checked
        .iter()
        .filter(|item| item.status == Status::Failed)
        .count();
    let mut out = String::from("### explainsql check\n\n");
    let headline = match (failed, checked.len()) {
        (0, _) => "No plan failed.".to_owned(),
        (1, 1) => "The plan failed.".to_owned(),
        (failed, total) => format!("{failed} of {total} plans failed."),
    };
    out.push_str(&format!("**{headline}** {}\n\n", check::summary(checked)));
    out.push_str("| Plan | Result | Shape | Why |\n|---|---|---|---|\n");
    for item in checked {
        let why = item
            .reasons
            .first()
            .or(item.notes.first())
            .map(|text| escape(text))
            .unwrap_or_default();
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            item.name.replace('`', "'"),
            match item.status {
                Status::Failed => "**Failed**",
                Status::New => "New",
                Status::Passed => "Passed",
            },
            match &item.diff {
                Some(diff) if !diff.shapes.same() => {
                    format!("`{}` → `{}`", diff.shapes.before, diff.shapes.after)
                }
                _ => format!("`{}`", item.shape),
            },
            why
        ));
    }
    for item in checked {
        let changed = item
            .diff
            .as_ref()
            .filter(|diff| !diff.shapes.same() || item.status == Status::Failed);
        if item.status != Status::Failed && changed.is_none() {
            continue;
        }
        out.push_str(&format!(
            "\n<details><summary><code>{}</code>: {}</summary>\n\n",
            escape(&item.name),
            if item.status == Status::Failed {
                "why it failed"
            } else {
                "what changed"
            }
        ));
        for reason in &item.reasons {
            out.push_str(&format!("- {}\n", escape(reason)));
        }
        if item.status == Status::Failed {
            for fix in fixes(item) {
                out.push_str(&format!("- **Fix:** {}\n", escape(&fix)));
            }
        }
        if let (Some(diff), Some(baseline)) = (changed, &item.baseline) {
            out.push('\n');
            out.push_str(&diff_markdown(baseline, &item.plan, diff));
        }
        out.push_str("\n</details>\n");
    }
    out
}

/// The checks of a CI run as JSON.
pub fn check_json(checked: &[Checked]) -> String {
    #[derive(Serialize)]
    struct Report<'a> {
        passed: bool,
        summary: String,
        plans: Vec<Entry<'a>>,
    }
    #[derive(Serialize)]
    struct Entry<'a> {
        #[serde(flatten)]
        checked: &'a Checked,
        findings: &'a [Finding],
        advice: &'a [Advice],
    }
    let report = Report {
        passed: check::passed(checked),
        summary: check::summary(checked),
        plans: checked
            .iter()
            .map(|item| Entry {
                checked: item,
                findings: &item.analysis.findings,
                advice: &item.analysis.advice,
            })
            .collect(),
    };
    let mut out = serde_json::to_string_pretty(&report).expect("the report serializes");
    out.push('\n');
    out
}

/// The checks of a CI run as SARIF 2.1.0, for code scanning: each finding,
/// and each plan worse than its locked one or changed from it. What fails
/// the run is an error; other findings are warnings or notes.
pub fn check_sarif(checked: &[Checked]) -> String {
    use serde_json::json;
    let mut rules: Vec<serde_json::Value> = crate::rules::RULES
        .iter()
        .map(|rule| {
            json!({
                "id": rule.id,
                "name": rule.name.replace(' ', ""),
                "shortDescription": {"text": rule.name},
                "helpUri": rule.doc_url(),
            })
        })
        .collect();
    rules.push(json!({
        "id": "plan-worse",
        "name": "PlanWorse",
        "shortDescription": {"text": "The plan is worse than its locked plan"},
    }));
    rules.push(json!({
        "id": "plan-changed",
        "name": "PlanChanged",
        "shortDescription": {"text": "The plan changed from its locked plan"},
    }));
    let location = |name: &str| {
        json!([{
            "physicalLocation": {
                "artifactLocation": {"uri": name},
                "region": {"startLine": 1},
            }
        }])
    };
    let mut results = Vec::new();
    for item in checked {
        for (index, finding) in item.analysis.findings.iter().enumerate() {
            let level = if item.failing.contains(&index) {
                "error"
            } else {
                match finding.severity {
                    Severity::High | Severity::Medium => "warning",
                    Severity::Low => "note",
                }
            };
            results.push(json!({
                "ruleId": finding.rule.id,
                "level": level,
                "message": {"text": format!("{}. {}", finding.summary, finding.action)},
                "locations": location(&item.name),
            }));
        }
        if let Some(diff) = &item.diff {
            let worse = item
                .reasons
                .iter()
                .any(|reason| reason.starts_with("Worse than the locked plan"));
            let (rule, level) = if worse {
                ("plan-worse", "error")
            } else if diff.shapes.same() {
                continue;
            } else if item.status == Status::Failed {
                ("plan-changed", "error")
            } else {
                ("plan-changed", "note")
            };
            results.push(json!({
                "ruleId": rule,
                "level": level,
                "message": {"text": diff.verdict},
                "locations": location(&item.name),
            }));
        }
    }
    let sarif = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "explainsql",
                    "informationUri": "https://github.com/onplt/explain-sql",
                    "rules": rules,
                }
            },
            "results": results,
        }],
    });
    let mut out = serde_json::to_string_pretty(&sarif).expect("SARIF serializes");
    out.push('\n');
    out
}

/// `shape 9208e2e2…, locked f155e550…`.
fn shape_note(item: &Checked) -> String {
    match &item.diff {
        Some(diff) if !diff.shapes.same() => {
            format!("shape {}, locked {}", diff.shapes.after, diff.shapes.before)
        }
        Some(_) => format!("shape {}, as locked", item.shape),
        None => format!("shape {}, not locked yet", item.shape),
    }
}

/// What the advice suggests for a plan that failed, with the test of each
/// index when it was tested.
fn fixes(item: &Checked) -> Vec<String> {
    item.analysis
        .advice
        .iter()
        .filter_map(|advice| match &advice.kind {
            AdviceKind::Index { ddl, .. } => Some(match proof_line(advice) {
                Some(proof) => format!("{ddl} {proof}"),
                None => ddl.clone(),
            }),
            AdviceKind::Rewrite { .. } | AdviceKind::ForeignKey { .. } => {
                Some(format!("{}: {}", advice.title(), advice.summary))
            }
            AdviceKind::AlreadyIndexed { .. } | AdviceKind::NoIndex { .. } => None,
        })
        .collect()
}

fn shapes_line(diff: &PlanDiff) -> String {
    if diff.shapes.same() {
        format!("Plan shape {} in both: the same plan.", diff.shapes.after)
    } else {
        format!(
            "Plan shape {} → {}: another plan.",
            diff.shapes.before, diff.shapes.after
        )
    }
}

/// `~ ` for a node that changed, `+ ` for a new one.
fn diff_marker(diff: &PlanDiff, id: NodeId) -> &'static str {
    if diff.added.contains(&id) {
        "+ "
    } else if diff.changes.iter().any(|change| change.after == Some(id)) {
        "~ "
    } else {
        "  "
    }
}

/// The nodes only the first plan has, the first few by name.
fn removed_labels(before: &Plan, diff: &PlanDiff) -> Vec<String> {
    let mut labels: Vec<String> = diff
        .removed
        .iter()
        .take(MAX_LISTED)
        .map(|&id| format::node(before.node(id)))
        .collect();
    if diff.removed.len() > MAX_LISTED {
        labels.push(format!("{} more", diff.removed.len() - MAX_LISTED));
    }
    labels
}

fn change_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Access => "Access",
        ChangeKind::Join => "Join",
        ChangeKind::JoinOrder => "Join order",
        ChangeKind::Strategy => "Strategy",
        ChangeKind::Added => "Added",
        ChangeKind::Removed => "Removed",
        ChangeKind::Spill => "Spill",
        ChangeKind::Estimate => "Estimate",
        ChangeKind::Work => "Work",
    }
}

/// The plan table, each row after its node's marker.
fn write_table(
    out: &mut String,
    rows: &[Row],
    paint: &Paint,
    marker: impl Fn(NodeId) -> &'static str,
) {
    let node_width = rows
        .iter()
        .map(|row| row.node.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let widths = [
        column_width(rows, "Share", |row| &row.share),
        column_width(rows, "Time", |row| &row.time),
        column_width(rows, "Rows", |row| &row.rows),
        column_width(rows, "Estimate", |row| &row.estimate),
        column_width(rows, "Buffers", |row| &row.buffers),
    ];
    let margin = rows
        .iter()
        .map(|row| marker(row.id).chars().count())
        .max()
        .unwrap_or(0);
    out.push_str(&paint.dim(&format!(
        "{:margin$}{:>w0$}  {:>w1$}  {:bar$}  {:node_width$}  {:>w2$}  {:>w3$}  {:>w4$}",
        "",
        "Share",
        "Time",
        "",
        "Node",
        "Rows",
        "Estimate",
        "Buffers",
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        bar = BAR_WIDTH,
    )));
    out.push('\n');
    for row in rows {
        let bar = paint.share(
            row.fraction,
            &format!("{:bar$}", bar(row.fraction), bar = BAR_WIDTH),
        );
        let padding = node_width - row.node.chars().count();
        let mark = marker(row.id);
        out.push_str(&paint.bold(mark));
        out.push_str(&" ".repeat(margin - mark.chars().count()));
        out.push_str(
            format!(
                "{:>w0$}  {:>w1$}  {bar}  {}{:padding$}  {:>w2$}  {:>w3$}  {:>w4$}",
                row.share,
                row.time,
                row.node,
                "",
                row.rows,
                row.estimate,
                row.buffers,
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3],
                w4 = widths[4],
            )
            .trim_end(),
        );
        out.push('\n');
    }
}

/// The report as Markdown, for an issue, a pull request or a chat.
pub fn markdown(plan: &Plan, analysis: &Analysis) -> String {
    let mut out = String::new();
    if let Some(sensitivity) = &analysis.parameters {
        markdown_parameters(&mut out, sensitivity);
        out.push_str("\n### The plan\n\n");
    }
    out.push_str(&format!("**{}**\n\n", escape(&analysis.verdict)));
    let facts = facts(plan, analysis);
    if !facts.is_empty() {
        out.push_str(&format!("{}\n\n", escape(&facts.join(" · "))));
    }
    out.push_str("| Share | Time | Node | Rows | Estimate | Buffers |\n");
    out.push_str("|---:|---:|---|---:|---:|---:|\n");
    for (row, (depth, _)) in table_rows(plan, analysis).iter().zip(plan.walk()) {
        let indent = "&nbsp;&nbsp;".repeat(depth);
        let arrow = if depth > 0 { "↳ " } else { "" };
        out.push_str(&format!(
            "| {} | {} | {indent}{arrow}{} | {} | {} | {} |\n",
            row.share,
            row.time,
            escape(&row.label),
            row.rows,
            row.estimate,
            row.buffers
        ));
    }
    out.push('\n');
    if analysis.findings.is_empty() {
        out.push_str("No findings: none of the rules found a problem in this plan.\n");
    } else {
        out.push_str("### Findings\n\n");
        for finding in &analysis.findings {
            out.push_str(&format!(
                "- **{} · [{}]({}) {}:** {}.\n",
                severity_name(finding.severity),
                finding.rule.id,
                finding.rule.doc_url(),
                finding.rule.name,
                escape(&finding.summary)
            ));
            for evidence in &finding.evidence {
                out.push_str(&format!(
                    "  - {}: `{}`\n",
                    evidence.label,
                    evidence.value.replace('`', "'")
                ));
            }
            out.push_str(&format!("  - **Action:** {}\n", escape(&finding.action)));
        }
    }
    markdown_advice(&mut out, plan, analysis);
    markdown_counterfactuals(&mut out, analysis);
    if !plan.warnings.is_empty() {
        out.push_str("\n### Parser warnings\n\n");
        for warning in &plan.warnings {
            out.push_str(&format!("- {}\n", escape(&warning.message)));
        }
    }
    out
}

/// The parsed plan and its analysis as JSON.
pub fn json(plan: &Plan, analysis: &Analysis) -> String {
    #[derive(Serialize)]
    struct Report<'a> {
        verdict: &'a str,
        findings: &'a [Finding],
        advice: &'a [Advice],
        #[serde(skip_serializing_if = "<[_]>::is_empty")]
        counterfactuals: &'a [Answer],
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<&'a Sensitivity>,
        metrics: &'a metrics::Metrics,
        plan: &'a Plan,
    }
    let report = Report {
        verdict: &analysis.verdict,
        findings: &analysis.findings,
        advice: &analysis.advice,
        counterfactuals: &analysis.counterfactuals,
        parameters: analysis.parameters.as_ref(),
        metrics: &analysis.metrics,
        plan,
    };
    let mut out = serde_json::to_string_pretty(&report).expect("the report serializes");
    out.push('\n');
    out
}

/// The advice section of the text report: suggestions, then the slow scans
/// no index would help.
fn text_advice(out: &mut String, plan: &Plan, analysis: &Analysis, paint: &Paint) {
    let (suggestions, explanations) = split_advice(&analysis.advice);
    if !suggestions.is_empty() {
        out.push('\n');
        out.push_str(&paint.bold(&format!("Advice ({})", suggestions.len())));
        out.push('\n');
        for advice in suggestions {
            out.push('\n');
            out.push_str(&format!(
                "  {}  {}\n",
                paint.confidence(advice.confidence),
                advice.title()
            ));
            if let AdviceKind::Index { ddl, .. } = &advice.kind {
                out.push_str(&format!("          {}\n", paint.bold(ddl)));
            }
            out.push_str(&wrap(&advice.summary, 10));
            out.push('\n');
            if let Some(line) = proof_line(advice) {
                out.push_str(&wrap(&line, 10));
                out.push('\n');
            }
            for caveat in &advice.caveats {
                out.push_str(&paint.dim(&wrap(&format!("! {caveat}"), 10)));
                out.push('\n');
            }
        }
    }
    if !explanations.is_empty() {
        out.push('\n');
        out.push_str(&paint.bold("No index would help"));
        out.push('\n');
        for advice in explanations {
            let node = advice.node.map(|id| format::node(plan.node(id)));
            if let (Some(node), AdviceKind::NoIndex { reason }) = (node, &advice.kind) {
                out.push_str(&wrap(&format!("{node}: {reason}"), 2));
                out.push('\n');
            }
        }
    }
}

fn markdown_advice(out: &mut String, plan: &Plan, analysis: &Analysis) {
    let (suggestions, explanations) = split_advice(&analysis.advice);
    if !suggestions.is_empty() {
        out.push_str("\n### Advice\n\n");
        for advice in suggestions {
            out.push_str(&format!(
                "- **{}** ({} confidence): {}\n",
                escape(&advice.title()),
                confidence_name(advice.confidence),
                escape(&advice.summary)
            ));
            if let AdviceKind::Index { ddl, .. } = &advice.kind {
                out.push_str(&format!("  ```sql\n  {ddl}\n  ```\n"));
            }
            if let Some(line) = proof_line(advice) {
                out.push_str(&format!("  - **{}**\n", escape(&line)));
            }
            for caveat in &advice.caveats {
                out.push_str(&format!("  - {}\n", escape(caveat)));
            }
        }
    }
    if !explanations.is_empty() {
        out.push_str("\n### No index would help\n\n");
        for advice in explanations {
            let node = advice.node.map(|id| format::node(plan.node(id)));
            if let (Some(node), AdviceKind::NoIndex { reason }) = (node, &advice.kind) {
                out.push_str(&format!("- {}: {}\n", escape(&node), escape(reason)));
            }
        }
    }
}

/// What the database said when asked why the planner chose its plan.
fn text_counterfactuals(out: &mut String, analysis: &Analysis, paint: &Paint) {
    if analysis.counterfactuals.is_empty() {
        return;
    }
    out.push('\n');
    out.push_str(&paint.bold(&format!(
        "Why not: the planner asked again ({})",
        analysis.counterfactuals.len()
    )));
    out.push('\n');
    for answer in &analysis.counterfactuals {
        out.push('\n');
        out.push_str(&format!(
            "  {}  {}\n",
            paint.verdict(answer.verdict),
            answer.question
        ));
        out.push_str(&wrap(&answer.summary, 15));
        out.push('\n');
        for evidence in &answer.evidence {
            out.push_str(&wrap(
                &format!("{}: {}", evidence.label, evidence.value),
                15,
            ));
            out.push('\n');
        }
        if let Some(action) = &answer.action {
            out.push_str(&wrap(&format!("→ {action}"), 15));
            out.push('\n');
        }
        out.push_str(&paint.dim(&wrap(&planned_with(answer), 15)));
        out.push('\n');
    }
}

fn markdown_counterfactuals(out: &mut String, analysis: &Analysis) {
    if analysis.counterfactuals.is_empty() {
        return;
    }
    out.push_str("\n### Why not: the planner asked again\n\n");
    for answer in &analysis.counterfactuals {
        out.push_str(&format!(
            "- **{} · {}** {}\n",
            answer.verdict.label(),
            escape(&answer.question),
            escape(&answer.summary)
        ));
        for evidence in &answer.evidence {
            out.push_str(&format!(
                "  - {}: `{}`\n",
                evidence.label,
                evidence.value.replace('`', "'")
            ));
        }
        if let Some(action) = &answer.action {
            out.push_str(&format!("  - **Action:** {}\n", escape(action)));
        }
        out.push_str(&format!("  - {}\n", escape(&planned_with(answer))));
    }
}

/// How the plan depends on the statement's parameters: the verdict, the
/// parameters, each value tried, whether PostgreSQL would switch to the
/// generic plan, what to do, and which plan the rest of the report shows.
fn text_parameters(out: &mut String, sensitivity: &Sensitivity, paint: &Paint) {
    out.push_str(&format!(
        "{}  {}\n",
        paint.bold("Parameters"),
        paint.sensitivity(sensitivity.verdict)
    ));
    out.push_str(&wrap(&sensitivity.summary, 2));
    out.push_str("\n\n");
    for parameter in &sensitivity.parameters {
        out.push_str(&wrap(
            &format!("${}  {}", parameter.number, parameter_facts(parameter)),
            2,
        ));
        out.push('\n');
    }
    if !sensitivity.rows.is_empty() {
        out.push('\n');
        let values: Vec<String> = sensitivity
            .rows
            .iter()
            .map(|row| tried_values(sensitivity, row))
            .collect();
        let width = values
            .iter()
            .map(|value| value.chars().count())
            .max()
            .unwrap_or(0)
            .min(32);
        for (row, value) in sensitivity.rows.iter().zip(&values) {
            let indent = 4 + width;
            let from = row
                .sample
                .as_ref()
                .map(params::Sample::describe)
                .unwrap_or_else(|| "the values given".to_owned());
            if value.chars().count() > width {
                out.push_str(&format!("  {value}\n"));
                out.push_str(&wrap(&from, indent));
            } else {
                out.push_str(&format!("  {value:<width$}  {from}"));
            }
            out.push('\n');
            let plan = if row.empty {
                paint.dim(NO_ROW)
            } else if row.generic {
                paint.dim("the generic plan")
            } else {
                custom_plan(row)
            };
            out.push_str(&wrap(&plan, indent));
            out.push('\n');
            if let Some(line) = measured_line(row) {
                out.push_str(&wrap(
                    &format!("measured, custom plan → generic plan: {line}"),
                    indent,
                ));
                out.push('\n');
            }
        }
    }
    if let Some(switch) = &sensitivity.switch {
        out.push('\n');
        out.push_str(&wrap(&switch.reason, 2));
        out.push('\n');
    }
    for advice in &sensitivity.advice {
        out.push_str(&wrap(&format!("→ {advice}"), 2));
        out.push('\n');
    }
    for note in &sensitivity.notes {
        out.push_str(&paint.dim(&wrap(note, 2)));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&wrap(&sensitivity.shown, 0));
    out.push('\n');
}

fn markdown_parameters(out: &mut String, sensitivity: &Sensitivity) {
    out.push_str(&format!(
        "### Parameters: {}\n\n{}\n\n",
        sensitivity.verdict.label(),
        escape(&sensitivity.summary)
    ));
    for parameter in &sensitivity.parameters {
        out.push_str(&format!(
            "- `${}` {}\n",
            parameter.number,
            escape(&parameter_facts(parameter))
        ));
    }
    if !sensitivity.rows.is_empty() {
        out.push_str("\n| Value | From | Custom plan | Measured, custom plan → generic plan |\n|---|---|---|---|\n");
        for row in &sensitivity.rows {
            let from = row
                .sample
                .as_ref()
                .map(params::Sample::describe)
                .unwrap_or_else(|| "the values given".to_owned());
            let plan = if row.empty {
                NO_ROW.to_owned()
            } else if row.generic {
                "the generic plan".to_owned()
            } else {
                custom_plan(row)
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                tried_values(sensitivity, row).replace('`', "'"),
                escape(&from),
                escape(&plan),
                escape(&measured_line(row).unwrap_or_default())
            ));
        }
    }
    out.push('\n');
    if let Some(switch) = &sensitivity.switch {
        out.push_str(&format!("{}\n\n", escape(&switch.reason)));
    }
    for advice in &sensitivity.advice {
        out.push_str(&format!("- **Action:** {}\n", escape(advice)));
    }
    for note in &sensitivity.notes {
        out.push_str(&format!("- {}\n", escape(note)));
    }
    out.push_str(&format!("\n{}\n", escape(&sensitivity.shown)));
}

/// A value that selects no row by its own logic, as a range that ends
/// before it starts.
const NO_ROW: &str = "no row: the planner proves it from the values alone";

/// `integer, compared with orders.customer_id (=), held at 4242`.
fn parameter_facts(parameter: &Parameter) -> String {
    let mut facts = Vec::new();
    if let Some(type_name) = &parameter.type_name {
        facts.push(type_name.clone());
    }
    facts.push(match (&parameter.column, parameter.clause) {
        (Some(column), _) => format!("compared with {} ({})", column.name(), column.operator),
        (None, Some(clause)) => format!("the {}", clause.keyword()),
        (None, None) => "not compared with a column".to_owned(),
    });
    facts.push(match &parameter.held {
        Some(value) => format!(
            "{} {}",
            if parameter.given { "given:" } else { "held at" },
            params::show_typed(Some(value), parameter.type_name.as_deref())
        ),
        None => "no value to try".to_owned(),
    });
    facts.join(", ")
}

/// The value a row tries, `$1 = 4242`, or all the values given.
fn tried_values(sensitivity: &Sensitivity, row: &params::Row) -> String {
    let shown: Vec<String> = sensitivity
        .parameters
        .iter()
        .zip(&row.values)
        .filter(|(parameter, _)| {
            row.parameter
                .is_none_or(|number| number == parameter.number)
        })
        .map(|(parameter, value)| parameter.show(value.as_deref()))
        .collect();
    shown.join(", ")
}

/// `Parallel Seq Scan on orders, cost 5,095`.
fn custom_plan(row: &params::Row) -> String {
    match row.plan.cost {
        Some(cost) => format!("{}, cost {}", row.plan.access, format::rows(cost.round())),
        None => row.plan.access.clone(),
    }
}

/// How the custom plan and then the generic plan did, measured with the
/// row's values.
fn measured_line(row: &params::Row) -> Option<String> {
    if let Some(timeout) = &row.timed_out {
        let custom = row
            .measured
            .as_ref()
            .and_then(|comparison| comparison.before.execution_time)
            .map(|time| format!(", while the custom plan took {}", format::duration(time)))
            .unwrap_or_default();
        return Some(format!(
            "the generic plan ran past the {timeout} timeout{custom}"
        ));
    }
    let comparison = row.measured.as_ref()?;
    Some(format!(
        "{}; {}",
        comparison.details(),
        match comparison.change {
            crate::compare::Change::Worse => "the generic plan does worse",
            crate::compare::Change::Better => "the generic plan does better",
            crate::compare::Change::Mixed => "neither does better on both",
            crate::compare::Change::Same | crate::compare::Change::Unknown => {
                "the generic plan does no worse"
            }
        }
    ))
}

/// `Planned again with enable_seqscan = off; measured, 3 runs each.`
pub fn planned_with(answer: &Answer) -> String {
    let settings: Vec<String> = answer.settings.iter().map(ToString::to_string).collect();
    let how = match &answer.comparison {
        Some(comparison) if answer.measured && comparison.after.runs > 1 => {
            format!("measured, {} runs each", comparison.after.runs)
        }
        _ if answer.measured => "measured".to_owned(),
        _ => "estimated".to_owned(),
    };
    let mut text = format!("Planned again with {}; {how}", settings.join(", "));
    if answer.approximate {
        text.push_str("; the settings changed other parts of the plan too");
    }
    text.push('.');
    text
}

/// `Tested with HypoPG: Estimated cost 4917 → 46 (107× cheaper)`.
fn proof_line(advice: &Advice) -> Option<String> {
    let proof = advice.proof.as_ref()?;
    let how = match advice.verification {
        Verification::Measured => "Measured with the index built and rolled back",
        _ => "Estimated with a hypothetical index (HypoPG)",
    };
    Some(format!("{how}: {}.", proof.details()))
}

/// Suggestions, and explanations of why no index would help.
fn split_advice(advice: &[Advice]) -> (Vec<&Advice>, Vec<&Advice>) {
    advice
        .iter()
        .partition(|advice| !matches!(advice.kind, AdviceKind::NoIndex { .. }))
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

/// Statement-level figures: planning and execution time, triggers, JIT,
/// buffers.
pub fn facts(plan: &Plan, analysis: &Analysis) -> Vec<String> {
    let statement = &analysis.metrics.statement;
    let mut facts = Vec::new();
    if let Some(time) = statement.planning_time {
        facts.push(format!("Planning {}", format::duration(time)));
    }
    if let Some(time) = statement.execution_time {
        facts.push(format!("Execution {}", format::duration(time)));
    }
    if let Some(time) = statement.trigger_time {
        facts.push(format!("Triggers {}", format::duration(time)));
    }
    if let Some(time) = statement.jit_time {
        facts.push(format!("JIT {}", format::duration(time)));
    }
    if let Some(time) = statement.unattributed_time.filter(|&time| {
        statement
            .total_time
            .is_some_and(|total| time >= 0.05 * total && time >= 1.0)
    }) {
        facts.push(format!("Outside the plan tree {}", format::duration(time)));
    }
    if let Some(buffers) = plan.root().buffers {
        let pages = metrics::blocks(&buffers);
        let mut text = format!(
            "Buffers {} pages",
            format::grouped(i64::try_from(pages).unwrap_or(i64::MAX))
        );
        let shared = buffers.shared_hit + buffers.shared_read;
        if shared > 0 {
            text.push_str(&format!(
                ", {} from cache",
                format::percent(buffers.shared_hit as f64 / shared as f64)
            ));
        }
        if buffers.temp_written > 0 {
            text.push_str(&format!(
                ", {} written to temporary files",
                format::kilobytes(buffers.temp_written as f64 * 8.0)
            ));
        }
        facts.push(text);
    }
    // I/O worth a mention: a tenth of the time, or without timing, a
    // millisecond.
    if let Some(io) = statement.io.filter(|io| {
        io.share
            .map_or(io.total() >= 1.0, |share| share >= MENTIONED_IO)
    }) {
        let mut text = format!("I/O {}", format::duration(io.total()));
        if let Some(share) = io.share {
            text.push_str(&format!(", {} of the time", format::percent(share)));
        }
        facts.push(text);
    }
    facts
}

/// One line of the plan table.
struct Row {
    id: NodeId,
    share: String,
    time: String,
    /// The tree guides and the label.
    node: String,
    label: String,
    rows: String,
    estimate: String,
    buffers: String,
    fraction: f64,
}

fn table_rows(plan: &Plan, analysis: &Analysis) -> Vec<Row> {
    tree_prefixes(plan)
        .into_iter()
        .map(|(id, prefix)| {
            let node = plan.node(id);
            let metrics = analysis.metrics.node(id);
            let never = node.actuals.is_some_and(|actuals| actuals.never_executed());
            let fraction = metrics.time_share.unwrap_or(0.0);
            let label = format::node(node);
            let room = MAX_NODE_WIDTH.saturating_sub(prefix.chars().count()).max(8);
            let shown = if label.chars().count() > room {
                let mut short: String = label.chars().take(room - 1).collect();
                short.push('…');
                short
            } else {
                label.clone()
            };
            let rows = match node.actuals {
                Some(actuals) if never => {
                    let _ = actuals;
                    "never run".to_owned()
                }
                Some(actuals) if actuals.loops > 1 => {
                    format!(
                        "{} ×{}",
                        format::rows(actuals.rows),
                        format::rows(actuals.loops as f64)
                    )
                }
                Some(actuals) => format::rows(actuals.rows),
                None => String::new(),
            };
            let estimate = match (node.estimates, metrics.misestimate) {
                (Some(estimates), Some(error)) if error.factor >= MARKED_MISESTIMATE => format!(
                    "{} {}{}",
                    format::rows(estimates.rows),
                    if error.underestimated { "▲" } else { "▼" },
                    format::factor(error.factor)
                ),
                (Some(estimates), _) => format::rows(estimates.rows),
                (None, _) => String::new(),
            };
            Row {
                id,
                share: metrics.time_share.map(format::percent).unwrap_or_default(),
                time: if never {
                    String::new()
                } else {
                    metrics
                        .exclusive_time
                        .map(format::duration)
                        .unwrap_or_default()
                },
                node: format!("{prefix}{shown}"),
                label,
                rows,
                estimate,
                buffers: metrics
                    .exclusive_buffers
                    .map(|buffers| {
                        format::grouped(
                            i64::try_from(metrics::blocks(&buffers)).unwrap_or(i64::MAX),
                        )
                    })
                    .unwrap_or_default(),
                fraction,
            }
        })
        .collect()
}

fn column_width(rows: &[Row], header: &str, cell: impl Fn(&Row) -> &String) -> usize {
    rows.iter()
        .map(|row| cell(row).chars().count())
        .max()
        .unwrap_or(0)
        .max(header.len())
}

/// Each node in pre-order with the tree guides that lead to it.
fn tree_prefixes(plan: &Plan) -> Vec<(NodeId, String)> {
    let mut rows = Vec::with_capacity(plan.nodes.len());
    let mut stack = vec![(NodeId(0), String::new(), String::new())];
    while let Some((id, own, below)) = stack.pop() {
        rows.push((id, own));
        let children = &plan.node(id).children;
        for (index, &child) in children.iter().enumerate().rev() {
            let last = index + 1 == children.len();
            stack.push((
                child,
                format!("{below}{}", if last { "└─ " } else { "├─ " }),
                format!("{below}{}", if last { "   " } else { "│  " }),
            ));
        }
    }
    rows
}

/// A bar of eighths of a character for a fraction, at most `BAR_WIDTH`
/// characters wide.
pub fn bar(fraction: f64) -> String {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    // Bounded by BAR_WIDTH × 8, so the conversion is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let eighths = (fraction.clamp(0.0, 1.0) * (BAR_WIDTH * 8) as f64).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        bar.push(EIGHTHS[eighths % 8]);
    }
    bar
}

/// Wraps text at word boundaries, indenting every line.
fn wrap(text: &str, indent: usize) -> String {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && indent + line.chars().count() + 1 + word.chars().count() > LINE_WIDTH
        {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    let pad = " ".repeat(indent);
    lines
        .iter()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::High => "High",
        Severity::Medium => "Medium",
        Severity::Low => "Low",
    }
}

/// Escapes what Markdown would interpret in table cells and text.
fn escape(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('*', "\\*")
        .replace('_', "\\_")
}

/// ANSI styling, or none.
struct Paint(bool);

impl Paint {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    fn share(&self, fraction: f64, text: &str) -> String {
        if fraction >= 0.5 {
            self.wrap("31", text)
        } else if fraction >= 0.2 {
            self.wrap("33", text)
        } else {
            text.to_owned()
        }
    }

    fn confidence(&self, confidence: Confidence) -> String {
        match confidence {
            Confidence::High => self.wrap("1;32", "SURE  "),
            Confidence::Medium => self.wrap("32", "LIKELY"),
            Confidence::Low => self.wrap("2", "MAYBE "),
        }
    }

    /// The verdict's tag, padded to line the questions up.
    fn verdict(&self, verdict: Verdict) -> String {
        let tag = format!("{:<11}", verdict.label());
        match verdict {
            Verdict::Misestimate | Verdict::CostSettings | Verdict::PlannerWrong => {
                self.wrap("1;31", &tag)
            }
            Verdict::Unusable => self.wrap("33", &tag),
            Verdict::PlannerRight | Verdict::MoreMemoryHelps => self.wrap("32", &tag),
            Verdict::Costlier | Verdict::MoreMemoryDoesNotHelp | Verdict::Inconclusive => {
                self.wrap("2", &tag)
            }
        }
    }

    /// A change's label, padded to line the summaries up.
    fn change(&self, kind: ChangeKind) -> String {
        let label = format!("{:<8}", kind.label());
        match kind {
            ChangeKind::Access | ChangeKind::Join | ChangeKind::JoinOrder => {
                self.wrap("1;33", &label)
            }
            ChangeKind::Spill | ChangeKind::Estimate => self.wrap("31", &label),
            ChangeKind::Strategy | ChangeKind::Added | ChangeKind::Removed => {
                self.wrap("33", &label)
            }
            ChangeKind::Work => self.wrap("2", &label),
        }
    }

    /// Whether the plan depends on the parameters' values.
    fn sensitivity(&self, verdict: params::Verdict) -> String {
        let label = verdict.label();
        match verdict {
            params::Verdict::Sensitive => self.wrap("1;31", label),
            params::Verdict::Insensitive | params::Verdict::Harmless => self.wrap("32", label),
            params::Verdict::Unknown => self.wrap("2", label),
        }
    }

    /// A check's result, padded to line the plans up.
    fn status(&self, status: Status) -> String {
        let label = format!("{:<4}", status.label());
        match status {
            Status::Failed => self.wrap("1;31", &label),
            Status::New => self.wrap("33", &label),
            Status::Passed => self.wrap("32", &label),
        }
    }

    fn severity(&self, severity: Severity) -> String {
        match severity {
            Severity::High => self.wrap("1;31", "HIGH  "),
            Severity::Medium => self.wrap("33", "MEDIUM"),
            Severity::Low => self.wrap("2", "LOW   "),
        }
    }
}
