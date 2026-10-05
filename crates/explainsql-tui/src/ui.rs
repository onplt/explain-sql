//! Drawing the viewer: the verdict on top, the plan tree, the details of the
//! selected node, the findings and a status line. Only the rows on screen are
//! drawn, so a frame costs the same for ten nodes as for ten thousand.

use explainsql_core::advisor::{Advice, AdviceKind, Confidence, Verification};
use explainsql_core::format;
use explainsql_core::ir::{Buffers, NodeId, PredicateKind};
use explainsql_core::metrics;
use explainsql_core::report;
use explainsql_core::rules::{Finding, Severity};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, Focus, Panel, Row};
use crate::theme::Theme;

/// From this width the details sit beside the tree rather than below it.
const SIDE_BY_SIDE: u16 = 110;
/// Misestimates from this factor are marked in the tree.
const MARKED_MISESTIMATE: f64 = 10.0;
/// The tree column is never narrower than this while other columns can go.
const MIN_NODE_WIDTH: usize = 30;
const BAR_WIDTH: usize = 10;
const SHARE_WIDTH: usize = 7;
const TIME_WIDTH: usize = 8;

pub fn draw(frame: &mut Frame, app: &mut App, theme: &Theme) {
    let area = frame.area();
    if area.width < 40 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("The terminal is too small for the viewer. Press q to quit.")
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let facts = report::facts(&app.plan, &app.analysis).join(" · ");
    let verdict_height = wrapped_lines(&app.analysis.verdict, area.width).min(3);
    let facts_height = wrapped_lines(&facts, area.width).min(2);
    let header_height = verdict_height + facts_height;
    let listed = match app.panel {
        Panel::Findings => app.analysis.findings.len(),
        Panel::Advice => app.analysis.advice.len(),
    };
    let findings = if app.analysis.findings.is_empty() && app.analysis.advice.is_empty() {
        0
    } else {
        let room = if area.height >= 30 { 6 } else { 4 };
        u16::try_from(listed.max(1)).unwrap_or(u16::MAX).min(room) + 1
    };
    let [top, body, bottom, status] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(6),
        Constraint::Length(findings),
        Constraint::Length(1),
    ])
    .areas(area);
    // Apart, so that a long verdict cannot push the figures off screen.
    let [verdict_area, facts_area] = Layout::vertical([
        Constraint::Length(verdict_height),
        Constraint::Length(facts_height),
    ])
    .areas(top);
    frame.render_widget(
        Paragraph::new(Line::styled(app.analysis.verdict.clone(), theme.title))
            .wrap(Wrap { trim: true }),
        verdict_area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(facts, theme.dim)).wrap(Wrap { trim: true }),
        facts_area,
    );

    let (tree, detail) = if area.width >= SIDE_BY_SIDE {
        let detail = (area.width * 35 / 100).clamp(40, 64);
        let [tree, detail] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(detail)]).areas(body);
        (tree, detail)
    } else {
        // The tree takes what it needs, up to 60%; the details the rest.
        let rows = u16::try_from(app.rows().len()).unwrap_or(u16::MAX);
        let tree = rows.saturating_add(2).min(body.height * 60 / 100);
        let [tree, detail] =
            Layout::vertical([Constraint::Length(tree), Constraint::Min(0)]).areas(body);
        (tree, detail)
    };
    draw_tree(frame, app, theme, tree);
    draw_detail(frame, app, theme, detail, area.width >= SIDE_BY_SIDE);
    if findings > 0 {
        match app.panel {
            Panel::Findings => draw_findings(frame, app, theme, bottom),
            Panel::Advice => draw_advice(frame, app, theme, bottom),
        }
    }
    draw_status(frame, app, theme, status);
    if app.help {
        draw_help(frame, theme, area);
    }
    if let Some(confirm) = &app.confirm {
        draw_confirm(frame, theme, area, &confirm.question);
    }
}

/// How many lines text takes when wrapped at word boundaries.
fn wrapped_lines(text: &str, width: u16) -> u16 {
    if text.is_empty() {
        return 0;
    }
    let width = usize::from(width).max(1);
    let (mut lines, mut column) = (1u16, 0);
    for word in text.split(' ') {
        let length = word.chars().count();
        if column > 0 && column + 1 + length > width {
            lines = lines.saturating_add(1);
            column = 0;
        }
        column += if column > 0 { 1 + length } else { length };
    }
    lines
}

/// What the time columns show for a node.
struct Measure {
    time: Option<f64>,
    fraction: Option<f64>,
}

fn measure(app: &App, id: NodeId) -> Measure {
    let node = app.plan.node(id);
    let metrics = app.analysis.metrics.node(id);
    let view = app.view;
    if view.buffers {
        let total = app
            .plan
            .root()
            .buffers
            .map(|buffers| metrics::blocks(&buffers));
        let own = if view.inclusive {
            node.buffers.map(|buffers| metrics::blocks(&buffers))
        } else {
            metrics
                .exclusive_buffers
                .map(|buffers| metrics::blocks(&buffers))
        };
        let time = if view.inclusive {
            metrics.inclusive_time
        } else {
            metrics.exclusive_time
        };
        return Measure {
            time,
            fraction: own
                .zip(total)
                .filter(|&(_, total)| total > 0)
                .map(|(own, total)| own as f64 / total as f64),
        };
    }
    let statement = &app.analysis.metrics.statement;
    match (view.inclusive, view.cpu) {
        (false, false) => Measure {
            time: metrics.exclusive_time,
            fraction: metrics.time_share,
        },
        (true, false) => Measure {
            time: metrics.inclusive_time,
            fraction: metrics
                .inclusive_time
                .zip(statement.execution_time.or(statement.tree_time))
                .filter(|&(_, total)| total > 0.0)
                .map(|(time, total)| (time / total).min(1.0)),
        },
        (inclusive, true) => {
            let time = if inclusive {
                metrics.inclusive_cpu_time
            } else {
                metrics.exclusive_cpu_time
            };
            // Including children, CPU time is that of the whole subtree:
            // a Gather's own figures cover only the leader.
            let time = if inclusive {
                time.map(|_| app.subtree_cpu(id))
            } else {
                time
            };
            let total = app.cpu_total();
            Measure {
                time,
                fraction: time
                    .filter(|_| total > 0.0)
                    .map(|time| (time / total).min(1.0)),
            }
        }
    }
}

/// The columns that do not depend on the view.
pub(crate) struct Counts {
    pub rows: String,
    pub estimate: String,
    pub buffers: String,
}

pub(crate) fn counts(app: &App, id: NodeId) -> Counts {
    let node = app.plan.node(id);
    let metrics = app.analysis.metrics.node(id);
    let never = node.actuals.is_some_and(|actuals| actuals.never_executed());
    let rows = match node.actuals {
        Some(_) if never => "never run".to_owned(),
        Some(actuals) if actuals.loops > 1 => format!(
            "{} ×{}",
            format::rows(actuals.rows),
            format::rows(actuals.loops as f64)
        ),
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
    Counts {
        rows,
        estimate,
        buffers: metrics
            .exclusive_buffers
            .map(|buffers| pages(&buffers))
            .unwrap_or_default(),
    }
}

fn pages(buffers: &Buffers) -> String {
    format::grouped(i64::try_from(metrics::blocks(buffers)).unwrap_or(i64::MAX))
}

/// Which optional columns fit, and how wide the node column is.
struct Columns {
    bar: bool,
    estimate: bool,
    buffers: bool,
    node: usize,
}

fn columns(app: &App, width: usize) -> Columns {
    let [rows, estimate, buffers] = app.widths();
    // Marker, share, time and rows, each followed by a space.
    let fixed = 2 + SHARE_WIDTH + 1 + TIME_WIDTH + 1 + rows + 1;
    let mut columns = Columns {
        bar: true,
        estimate: true,
        buffers: buffers > 0,
        node: 0,
    };
    let used = |columns: &Columns| {
        fixed
            + if columns.bar { BAR_WIDTH + 1 } else { 0 }
            + if columns.estimate { estimate + 1 } else { 0 }
            + if columns.buffers { buffers + 1 } else { 0 }
    };
    for drop in [
        |columns: &mut Columns| columns.buffers = false,
        |columns: &mut Columns| columns.bar = false,
        |columns: &mut Columns| columns.estimate = false,
    ] {
        if width.saturating_sub(used(&columns)) >= MIN_NODE_WIDTH {
            break;
        }
        drop(&mut columns);
    }
    columns.node = width.saturating_sub(used(&columns)).max(8);
    columns
}

fn draw_tree(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let mode = match (app.view.buffers, app.view.inclusive, app.view.cpu) {
        (true, false, _) => "buffers read by each node",
        (true, true, _) => "buffers including children",
        (false, false, false) => "time in each node",
        (false, true, false) => "time including children",
        (false, false, true) => "CPU time in each node",
        (false, true, true) => "CPU time including children",
    };
    let focused = app.focus == Focus::Tree;
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(if focused {
            theme.focused_border
        } else {
            theme.border
        })
        .title(Line::from(vec![
            Span::styled(" Plan ", theme.title),
            Span::styled(format!("· {mode} "), theme.dim),
            Span::styled(
                match &app.live {
                    Some(live) if !live.measured => "· estimated: not run yet ",
                    Some(_) => "· measured with EXPLAIN ANALYZE, rolled back ",
                    None => "",
                },
                theme.warm,
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let width = usize::from(inner.width);
    let columns = columns(app, width);
    let [rows_w, estimate_w, buffers_w] = app.widths();

    let mut header = format!(
        "  {:>SHARE_WIDTH$} {:>TIME_WIDTH$} ",
        if app.view.buffers { "Pages" } else { "Share" },
        "Time"
    );
    if columns.bar {
        header.push_str(&" ".repeat(BAR_WIDTH + 1));
    }
    header.push_str(&format!(
        "{:<w$} {:>rows_w$}",
        "Node",
        "Rows",
        w = columns.node
    ));
    if columns.estimate {
        header.push_str(&format!(" {:>estimate_w$}", "Estimate"));
    }
    if columns.buffers {
        header.push_str(&format!(" {:>buffers_w$}", "Buffers"));
    }
    let mut lines = vec![Line::styled(header, theme.dim)];

    let height = usize::from(inner.height) - 1;
    app.tree_height = height;
    app.scroll_into_view(height);
    let matches = app.matches();
    let end = (app.offset + height).min(app.rows().len());
    for index in app.offset..end {
        let row = &app.rows()[index];
        let selected = index == app.selected;
        lines.push(tree_line(app, theme, row, &columns, selected, &matches));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn tree_line<'a>(
    app: &App,
    theme: &Theme,
    row: &Row,
    columns: &Columns,
    selected: bool,
    matches: &[NodeId],
) -> Line<'a> {
    let [rows_w, estimate_w, buffers_w] = app.widths();
    let members: &[NodeId] = row
        .group
        .as_deref()
        .unwrap_or(std::slice::from_ref(&row.node));
    let flagged = app
        .analysis
        .findings
        .iter()
        .filter_map(|finding| {
            finding
                .node
                .filter(|node| members.contains(node))
                .map(|_| finding)
        })
        .map(|finding| finding.severity)
        .min();

    // Groups add up their members.
    let (mut time, mut fraction, mut any_time, mut any_fraction) = (0.0, 0.0, false, false);
    for &id in members {
        let measure = measure(app, id);
        if let Some(value) = measure.time {
            time += value;
            any_time = true;
        }
        if let Some(value) = measure.fraction {
            fraction += value;
            any_fraction = true;
        }
    }
    let node = app.plan.node(row.node);
    let never = row.group.is_none() && node.actuals.is_some_and(|actuals| actuals.never_executed());
    let counts = if row.group.is_some() {
        group_counts(app, members)
    } else {
        counts(app, row.node)
    };

    let mut spans = Vec::new();
    spans.push(match flagged {
        Some(Severity::High) => Span::styled("! ", theme.high),
        Some(Severity::Medium) => Span::styled("! ", theme.medium),
        Some(Severity::Low) => Span::styled("· ", theme.low),
        None => Span::raw("  "),
    });
    let share = if !any_fraction {
        String::new()
    } else if app.view.buffers {
        format::percent(fraction)
    } else {
        format::percent(fraction.min(1.0))
    };
    spans.push(Span::styled(
        format!("{share:>SHARE_WIDTH$} "),
        theme.share(fraction),
    ));
    let time = if any_time && !never {
        format::duration(time)
    } else {
        String::new()
    };
    spans.push(Span::raw(format!("{time:>TIME_WIDTH$} ")));
    if columns.bar {
        spans.push(Span::styled(
            format!("{:BAR_WIDTH$} ", report::bar(fraction)),
            theme.share(fraction),
        ));
    }

    let suffix = match &row.group {
        Some(group) => format!("  +{} similar", group.len() - 1),
        None => String::new(),
    };
    let glyph = if row.group.is_some() || row.collapsed {
        "▸ "
    } else {
        ""
    };
    let room = columns.node.saturating_sub(
        row.prefix.chars().count() + glyph.chars().count() + suffix.chars().count(),
    );
    let label = fit(app.label(row.node), room) + &suffix;
    let padding = columns
        .node
        .saturating_sub(row.prefix.chars().count() + glyph.chars().count() + label.chars().count());
    spans.push(Span::styled(row.prefix.clone(), theme.dim));
    spans.push(Span::styled(glyph, theme.key));
    let matched = members.iter().any(|id| matches.contains(id));
    spans.push(Span::styled(
        label,
        if matched { theme.matched } else { theme.text },
    ));
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(Span::raw(format!(" {:>rows_w$}", counts.rows)));
    if columns.estimate {
        let marked = counts.estimate.contains('▲') || counts.estimate.contains('▼');
        spans.push(Span::styled(
            format!(" {:>estimate_w$}", counts.estimate),
            if marked { theme.warm } else { theme.text },
        ));
    }
    if columns.buffers {
        spans.push(Span::raw(format!(" {:>buffers_w$}", counts.buffers)));
    }
    let line = Line::from(spans);
    if selected {
        line.style(theme.selected)
    } else {
        line
    }
}

fn group_counts(app: &App, members: &[NodeId]) -> Counts {
    let (mut rows, mut estimate, mut blocks) = (0.0, 0.0, 0u64);
    let (mut any_rows, mut any_estimate, mut any_blocks) = (false, false, false);
    for &id in members {
        let node = app.plan.node(id);
        if let Some(actuals) = node.actuals {
            rows += actuals.rows * actuals.loops as f64;
            any_rows = true;
        }
        if let Some(estimates) = node.estimates {
            estimate += estimates.rows;
            any_estimate = true;
        }
        if let Some(buffers) = app.analysis.metrics.node(id).exclusive_buffers {
            blocks += metrics::blocks(&buffers);
            any_blocks = true;
        }
    }
    let show = |any: bool, value: f64| {
        if any {
            format::rows(value.round())
        } else {
            String::new()
        }
    };
    Counts {
        rows: show(any_rows, rows),
        estimate: show(any_estimate, estimate),
        buffers: show(any_blocks, blocks as f64),
    }
}

/// Shortens text to `width` characters, ending with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut short: String = text.chars().take(width - 1).collect();
    short.push('…');
    short
}

fn draw_detail(frame: &mut Frame, app: &App, theme: &Theme, area: Rect, beside: bool) {
    let (title, lines) = match app.focus {
        Focus::Findings => match app.analysis.findings.get(app.finding) {
            Some(finding) => (" Finding ", finding_lines(app, theme, finding)),
            None => (" Details ", Vec::new()),
        },
        Focus::Advice => match app.analysis.advice.get(app.advice) {
            Some(advice) => (" Advice ", advice_lines(app, theme, advice)),
            None => (" Details ", Vec::new()),
        },
        Focus::Tree => (" Details ", node_lines(app, theme)),
    };
    let block = Block::new()
        .borders(if beside {
            Borders::TOP | Borders::LEFT
        } else {
            Borders::TOP
        })
        .border_style(theme.border)
        .title(Span::styled(title, theme.title));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        area,
    );
}

fn field<'a>(theme: &Theme, label: &str, value: String) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme.dim),
        Span::raw(value),
    ])
}

fn node_lines<'a>(app: &App, theme: &Theme) -> Vec<Line<'a>> {
    let row = app.selected_row();
    let id = row.node;
    let node = app.plan.node(id);
    let metrics = app.analysis.metrics.node(id);
    let mut lines = vec![Line::styled(app.label(id).to_owned(), theme.title)];
    if let Some(group) = &row.group {
        lines.push(Line::styled(
            format!(
                "One of {} similar siblings, shown as one row; press l to list them.",
                group.len()
            ),
            theme.dim,
        ));
    }
    if let Some(relationship) = node.relationship {
        lines.push(field(theme, "Role", format!("{relationship:?}")));
    }
    if let Some(time) = metrics.exclusive_time {
        let mut text = format::duration(time);
        if let Some(share) = metrics.time_share {
            text.push_str(&format!(" ({} of the runtime)", format::percent(share)));
        }
        lines.push(field(theme, "Time in node", text));
    }
    if let Some(time) = metrics.inclusive_time {
        lines.push(field(theme, "With children", format::duration(time)));
    }
    if metrics.processes > 1.0 {
        if let Some(cpu) = metrics.exclusive_cpu_time {
            lines.push(field(
                theme,
                "CPU time",
                format!(
                    "{} over {} processes",
                    format::duration(cpu),
                    format::rows(metrics.processes)
                ),
            ));
        }
    }
    if let Some(actuals) = node.actuals {
        if actuals.never_executed() {
            lines.push(field(theme, "Rows", "never executed".to_owned()));
        } else {
            let mut text = format!("{} per loop", format::rows(actuals.rows));
            if actuals.loops > 1 {
                text.push_str(&format!(
                    " × {} loops = {}",
                    format::rows(actuals.loops as f64),
                    format::rows((actuals.rows * actuals.loops as f64).round())
                ));
            }
            lines.push(field(theme, "Rows", text));
        }
    }
    if let Some(estimates) = node.estimates {
        let mut text = format!("{} rows", format::rows(estimates.rows));
        if let Some(error) = metrics.misestimate.filter(|error| error.factor >= 2.0) {
            text.push_str(&format!(
                ", {} {}",
                format::factor(error.factor),
                if error.underestimated {
                    "more in fact"
                } else {
                    "fewer in fact"
                }
            ));
        }
        text.push_str(&format!(
            "; cost {:.2}..{:.2}, width {}",
            estimates.startup_cost, estimates.total_cost, estimates.width
        ));
        lines.push(field(theme, "Estimate", text));
    }
    for (label, removed) in [
        ("Removed by filter", node.rows_removed_by_filter),
        ("Removed by join filter", node.rows_removed_by_join_filter),
        ("Removed by recheck", node.rows_removed_by_index_recheck),
    ] {
        if removed > 0.0 {
            lines.push(field(
                theme,
                label,
                format!("{} per loop", format::rows(removed)),
            ));
        }
    }
    if let (Some(planned), Some(launched)) = (node.workers_planned, node.workers_launched) {
        lines.push(field(
            theme,
            "Workers",
            format!("{launched} launched of {planned} planned"),
        ));
    }
    if let Some(buffers) = node.buffers {
        lines.push(field(theme, "Buffers", buffer_text(&buffers)));
        if let Some(own) = metrics.exclusive_buffers {
            lines.push(field(
                theme,
                "Read by the node itself",
                format!("{} pages", pages(&own)),
            ));
        }
    }
    let predicates: Vec<_> = PredicateKind::ALL
        .iter()
        .flat_map(|&kind| {
            node.predicates
                .iter()
                .filter(move |predicate| predicate.kind == kind)
        })
        .collect();
    if !predicates.is_empty() {
        lines.push(Line::raw(""));
        for predicate in predicates {
            lines.push(field(theme, predicate.kind.key(), predicate.text.clone()));
        }
    }
    for (label, keys) in [
        ("Sort Key", &node.sort_key),
        ("Presorted Key", &node.presorted_key),
        ("Group Key", &node.group_key),
        ("Output", &node.output),
    ] {
        if !keys.is_empty() {
            lines.push(field(theme, label, keys.join(", ")));
        }
    }
    if !node.extra.is_empty() {
        lines.push(Line::raw(""));
        for (key, value) in &node.extra {
            let value = match value.as_str() {
                Some(text) => text.to_owned(),
                None => value.to_string(),
            };
            lines.push(field(theme, key, value));
        }
    }
    let findings: Vec<&Finding> = app
        .analysis
        .findings
        .iter()
        .filter(|finding| finding.node == Some(id))
        .collect();
    for finding in findings {
        lines.push(Line::raw(""));
        lines.extend(finding_lines(app, theme, finding));
    }
    lines
}

fn buffer_text(buffers: &Buffers) -> String {
    let mut parts = Vec::new();
    for (label, value) in [
        ("shared hit", buffers.shared_hit),
        ("read", buffers.shared_read),
        ("dirtied", buffers.shared_dirtied),
        ("written", buffers.shared_written),
        ("local hit", buffers.local_hit),
        ("local read", buffers.local_read),
        ("temp read", buffers.temp_read),
        ("temp written", buffers.temp_written),
    ] {
        if value > 0 {
            parts.push(format!(
                "{label} {}",
                format::grouped(i64::try_from(value).unwrap_or(i64::MAX))
            ));
        }
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(", ")
    }
}

fn severity<'a>(theme: &Theme, severity: Severity) -> Span<'a> {
    match severity {
        Severity::High => Span::styled("HIGH  ", theme.high),
        Severity::Medium => Span::styled("MEDIUM", theme.medium),
        Severity::Low => Span::styled("LOW   ", theme.low),
    }
}

fn finding_lines<'a>(app: &App, theme: &Theme, finding: &Finding) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(vec![
        severity(theme, finding.severity),
        Span::styled(
            format!(" {} {}", finding.rule.id, finding.rule.name),
            theme.title,
        ),
    ])];
    if let Some(node) = finding.node {
        lines.push(Line::styled(format!("On {}", app.label(node)), theme.dim));
    }
    lines.push(Line::raw(format!("{}.", finding.summary)));
    for evidence in &finding.evidence {
        lines.push(field(theme, evidence.label, evidence.value.clone()));
    }
    lines.push(Line::from(vec![
        Span::styled("→ ", theme.key),
        Span::raw(finding.action.clone()),
    ]));
    lines.push(field(theme, "Docs", finding.rule.doc_url()));
    lines
}

fn draw_findings(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Findings;
    let count = app.analysis.findings.len();
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(if focused {
            theme.focused_border
        } else {
            theme.border
        })
        .title(panel_title(app, theme, focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height).max(1);
    if count == 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No findings: none of the rules found a problem. Press i for the advice.",
                theme.dim,
            )),
            inner,
        );
        return;
    }
    if app.finding < app.findings_offset {
        app.findings_offset = app.finding;
    } else if app.finding >= app.findings_offset + height {
        app.findings_offset = app.finding + 1 - height;
    }
    let width = usize::from(inner.width);
    let lines: Vec<Line> = app
        .analysis
        .findings
        .iter()
        .enumerate()
        .skip(app.findings_offset)
        .take(height)
        .map(|(index, finding)| {
            let head = format!(" {} {}: ", finding.rule.id, finding.rule.name);
            let used = 6 + head.chars().count();
            let line = Line::from(vec![
                severity(theme, finding.severity),
                Span::styled(head, theme.title),
                Span::raw(fit(&finding.summary, width.saturating_sub(used))),
            ]);
            if focused && index == app.finding {
                line.style(theme.selected)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// `Findings (2) · Advice (1)`, the shown list first, with what Enter does
/// when it has the focus.
fn panel_title<'a>(app: &App, theme: &Theme, focused: bool) -> Line<'a> {
    let findings = format!(" Findings ({}) ", app.analysis.findings.len());
    let advice = format!(" Advice ({}) ", app.analysis.advice.len());
    let (findings_style, advice_style) = match app.panel {
        Panel::Findings => (theme.title, theme.dim),
        Panel::Advice => (theme.dim, theme.title),
    };
    let hint = if focused {
        "· Enter: go to the node "
    } else {
        "· Tab to browse, f/i to switch "
    };
    Line::from(vec![
        Span::styled(findings, findings_style),
        Span::styled("│", theme.dim),
        Span::styled(advice, advice_style),
        Span::styled(hint, theme.dim),
    ])
}

fn confidence<'a>(theme: &Theme, advice: &Advice) -> Span<'a> {
    match (&advice.kind, advice.confidence) {
        (AdviceKind::NoIndex { .. }, _) => Span::styled("NO    ", theme.low),
        (_, Confidence::High) => Span::styled("SURE  ", theme.good),
        (_, Confidence::Medium) => Span::styled("LIKELY", theme.good),
        (_, Confidence::Low) => Span::styled("MAYBE ", theme.low),
    }
}

fn draw_advice(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let focused = app.focus == Focus::Advice;
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(if focused {
            theme.focused_border
        } else {
            theme.border
        })
        .title(panel_title(app, theme, focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.analysis.advice.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No advice: no scan in this plan calls for an index.",
                theme.dim,
            )),
            inner,
        );
        return;
    }
    let height = usize::from(inner.height).max(1);
    if app.advice < app.advice_offset {
        app.advice_offset = app.advice;
    } else if app.advice >= app.advice_offset + height {
        app.advice_offset = app.advice + 1 - height;
    }
    let width = usize::from(inner.width);
    let lines: Vec<Line> = app
        .analysis
        .advice
        .iter()
        .enumerate()
        .skip(app.advice_offset)
        .take(height)
        .map(|(index, advice)| {
            let head = match &advice.kind {
                AdviceKind::NoIndex { .. } => String::new(),
                _ => format!(" {}: ", advice.title()),
            };
            let text = match (&advice.kind, &advice.proof) {
                (AdviceKind::Index { .. }, Some(proof)) => format!("tested: {}", proof.summary()),
                (AdviceKind::Index { ddl, .. }, None) => ddl.clone(),
                _ => advice.summary.clone(),
            };
            let head = if head.is_empty() {
                " ".to_owned()
            } else {
                head
            };
            let used = 6 + head.chars().count();
            let line = Line::from(vec![
                confidence(theme, advice),
                Span::styled(head, theme.title),
                Span::raw(fit(&text, width.saturating_sub(used))),
            ]);
            if focused && index == app.advice {
                line.style(theme.selected)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn advice_lines<'a>(app: &App, theme: &Theme, advice: &Advice) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(vec![
        confidence(theme, advice),
        Span::styled(format!(" {}", advice.title()), theme.title),
    ])];
    if let Some(node) = advice.node {
        lines.push(Line::styled(format!("On {}", app.label(node)), theme.dim));
    }
    if let AdviceKind::Index { ddl, .. } = &advice.kind {
        lines.push(Line::raw(""));
        lines.push(Line::styled(ddl.clone(), theme.good));
        lines.push(Line::styled("Press c to copy it.", theme.dim));
        lines.push(Line::raw(""));
    }
    lines.push(Line::raw(advice.summary.clone()));
    for evidence in &advice.evidence {
        lines.push(field(theme, evidence.label, evidence.value.clone()));
    }
    for caveat in &advice.caveats {
        lines.push(Line::from(vec![
            Span::styled("! ", theme.warm),
            Span::styled(caveat.clone(), theme.dim),
        ]));
    }
    if let Some(proof) = &advice.proof {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Before → after: {}", proof.summary()),
            if proof.improved() {
                theme.good
            } else {
                theme.warm
            },
        ));
        if !proof.new_indexes.is_empty() {
            lines.push(field(theme, "The plan uses", proof.new_indexes.join(", ")));
        }
    }
    if matches!(advice.kind, AdviceKind::Index { .. })
        && app.live.is_some()
        && advice.proof.is_none()
    {
        lines.push(Line::styled("Press t to test it.", theme.dim));
    }
    if !matches!(advice.kind, AdviceKind::NoIndex { .. }) {
        let status = match advice.verification {
            Verification::Unverified => "not verified: from the plan alone",
            Verification::Estimated => "estimated with a hypothetical index (HypoPG)",
            Verification::Measured => "measured with the index created and rolled back",
        };
        lines.push(field(theme, "Verification", status.to_owned()));
    }
    lines
}

fn draw_status(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let line = match (&app.search, &app.message) {
        (Some(search), _) if search.editing => Line::from(vec![
            Span::styled("/", theme.key),
            Span::raw(search.query.clone()),
            Span::styled("▏", theme.key),
            Span::styled("  Enter: find  Esc: cancel", theme.dim),
        ]),
        _ if app.live.as_ref().is_some_and(|live| live.running.is_some()) => {
            let (task, database, elapsed) = app
                .live
                .as_ref()
                .and_then(|live| {
                    Some((
                        live.task.as_str(),
                        live.database.as_str(),
                        live.running?.elapsed(),
                    ))
                })
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{task} "), theme.warm),
                Span::styled(format!("on {database}… "), theme.dim),
                Span::raw(format!("{:.1} s", elapsed.as_secs_f64())),
                Span::styled("  Esc", theme.key),
                Span::styled(" cancel", theme.dim),
            ])
        }
        (_, Some(message)) => Line::raw(message.clone()),
        _ => {
            let mut spans = Vec::new();
            let connected = app.live.is_some();
            for (key, what) in [
                ("j/k", "move"),
                ("h/l", "fold"),
                ("/", "search"),
                ("1-9", "hotspots"),
                ("Tab", "list"),
                ("i", "advice"),
                ("r", "run"),
                ("t", "test"),
                ("?", "help"),
                ("q", "quit"),
            ] {
                if !connected && (key == "r" || key == "t") {
                    continue;
                }
                spans.push(Span::styled(key, theme.key));
                spans.push(Span::styled(format!(" {what}  "), theme.dim));
            }
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

const HELP: [(&str, &str); 22] = [
    ("j k ↓ ↑", "Move"),
    ("PgDn PgUp", "Move a page"),
    ("g G", "First, last node"),
    ("h l ← →", "Fold, unfold (h on a leaf: go to its parent)"),
    ("Enter Space", "Fold or unfold"),
    ("/", "Search node names and conditions"),
    ("n N", "Next, previous match"),
    ("1 … 9", "Go to the slowest nodes"),
    ("f i", "Show the findings, the advice"),
    ("Tab", "Switch between the plan and the list"),
    ("Enter", "In the list: go to the node"),
    ("c", "Copy the suggested CREATE INDEX"),
    ("x", "Time in the node, or including its children"),
    ("w", "Wall-clock or CPU time (parallel plans)"),
    ("b", "Time or buffers"),
    ("J K", "Scroll the details"),
    ("r e", "Connected: run again, edit the statement"),
    ("t", "Connected: test the suggested index"),
    ("Esc", "Connected: cancel a run"),
    ("?", "This help"),
    ("q Esc", "Quit"),
    ("", "Any key closes this help."),
];

fn draw_confirm(frame: &mut Frame, theme: &Theme, area: Rect, question: &str) {
    let width = area.width.min(60);
    let height = (wrapped_lines(question, width.saturating_sub(4)) + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(question.to_owned())
            .wrap(Wrap { trim: true })
            .block(
                Block::bordered()
                    .border_style(theme.focused_border)
                    .title(Span::styled(" Confirm ", theme.title)),
            ),
        popup,
    );
}

fn draw_help(frame: &mut Frame, theme: &Theme, area: Rect) {
    let width = area.width.min(64);
    let height = area.height.min(HELP.len() as u16 + 2);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!(" {keys:<12}"), theme.key),
                Span::styled((*what).to_owned(), Style::new()),
            ])
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(theme.focused_border)
                .title(Span::styled(" Keys ", theme.title)),
        ),
        popup,
    );
}
