//! Static reports of an analysis: text for terminals, Markdown for issues
//! and pull requests, JSON for other programs.

use serde::Serialize;

use crate::advisor::{Advice, AdviceKind, Confidence, Verification};
use crate::analysis::Analysis;
use crate::format;
use crate::ir::{NodeId, Plan};
use crate::metrics;
use crate::rules::{Finding, Severity};

/// Node labels longer than this are shortened in the plan table.
const MAX_NODE_WIDTH: usize = 64;
/// Where text is wrapped.
const LINE_WIDTH: usize = 100;
const BAR_WIDTH: usize = 10;

/// Misestimates from this factor are marked in the plan table.
const MARKED_MISESTIMATE: f64 = 10.0;

/// The report for a terminal. `color` adds ANSI colors.
pub fn text(plan: &Plan, analysis: &Analysis, color: bool) -> String {
    let paint = Paint(color);
    let mut out = String::new();
    out.push_str(&paint.bold(&wrap(&analysis.verdict, 0)));
    out.push_str("\n\n");
    let facts = facts(plan, analysis);
    if !facts.is_empty() {
        out.push_str(&paint.dim(&wrap(&facts.join(" · "), 0)));
        out.push_str("\n\n");
    }

    let rows = table_rows(plan, analysis);
    let node_width = rows
        .iter()
        .map(|row| row.node.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let widths = [
        column_width(&rows, "Share", |row| &row.share),
        column_width(&rows, "Time", |row| &row.time),
        column_width(&rows, "Rows", |row| &row.rows),
        column_width(&rows, "Estimate", |row| &row.estimate),
        column_width(&rows, "Buffers", |row| &row.buffers),
    ];
    out.push_str(&paint.dim(&format!(
        "{:>w0$}  {:>w1$}  {:bar$}  {:node_width$}  {:>w2$}  {:>w3$}  {:>w4$}",
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
    for row in &rows {
        let bar = paint.share(
            row.fraction,
            &format!("{:bar$}", bar(row.fraction), bar = BAR_WIDTH),
        );
        let padding = node_width - row.node.chars().count();
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

/// The report as Markdown, for an issue, a pull request or a chat.
pub fn markdown(plan: &Plan, analysis: &Analysis) -> String {
    let mut out = format!("**{}**\n\n", escape(&analysis.verdict));
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
                "- **{} · {} {}:** {}.\n",
                severity_name(finding.severity),
                finding.rule.id,
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
        metrics: &'a metrics::Metrics,
        plan: &'a Plan,
    }
    let report = Report {
        verdict: &analysis.verdict,
        findings: &analysis.findings,
        advice: &analysis.advice,
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

/// `Tested with HypoPG: Estimated cost 4917 → 46 (107× cheaper)`.
fn proof_line(advice: &Advice) -> Option<String> {
    let proof = advice.proof.as_ref()?;
    let how = match advice.verification {
        Verification::Measured => "Measured with the index built and rolled back",
        _ => "Estimated with a hypothetical index (HypoPG)",
    };
    Some(format!("{how}: {}.", proof.summary()))
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
    facts
}

/// One line of the plan table.
struct Row {
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

    fn severity(&self, severity: Severity) -> String {
        match severity {
            Severity::High => self.wrap("1;31", "HIGH  "),
            Severity::Medium => self.wrap("33", "MEDIUM"),
            Severity::Low => self.wrap("2", "LOW   "),
        }
    }
}
