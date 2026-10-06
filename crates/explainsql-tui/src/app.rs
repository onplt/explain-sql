//! The viewer's state and what the keys do to it, independent of the
//! terminal so that it can be tested directly.

use std::collections::HashSet;

use explainsql_core::Analysis;
use explainsql_core::advisor::AdviceKind;
use explainsql_core::fingerprint::leaf_key;
use explainsql_core::format;
use explainsql_core::ir::{NodeId, Plan};

use crate::icicle::{self, Cell, Weights};

/// Runs of at least this many similar siblings are shown as one row.
const MIN_GROUP: usize = 4;

/// One line of the plan tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The node, or the first node of a group.
    pub node: NodeId,
    /// Tree guides leading to the node: `│  ├─ `.
    pub prefix: String,
    /// For a group of similar siblings: all of them, in plan order.
    pub group: Option<Vec<NodeId>>,
    pub has_children: bool,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Findings,
    Advice,
}

/// What the panel under the tree lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Findings,
    Advice,
}

/// What the Time, Share and bar columns show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// Time in the node and below it, rather than in the node itself.
    pub inclusive: bool,
    /// CPU time summed over parallel processes, rather than wall-clock time.
    pub cpu: bool,
    /// Bars and shares by buffers rather than by time.
    pub buffers: bool,
}

/// What a key asks the event loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
    /// Put this text on the clipboard.
    Copy(String),
    /// Run the statement again with EXPLAIN ANALYZE (connected mode).
    Run,
    /// Open the statement in the editor, then run it (connected mode).
    Edit,
    /// Stop the running statement (connected mode).
    Cancel,
    /// Test an index suggestion (connected mode).
    Prove {
        ddl: String,
        measured: bool,
    },
    /// Ask the planner why it chose what it chose for a node (connected
    /// mode).
    WhyNot {
        node: NodeId,
    },
}

/// The state of connected mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    /// `user@host:port/dbname`.
    pub database: String,
    pub sql: String,
    /// The plan shown was measured with EXPLAIN ANALYZE, not estimated.
    pub measured: bool,
    /// A run in progress, and since when.
    pub running: Option<std::time::Instant>,
    /// What is running: `Running EXPLAIN ANALYZE`.
    pub task: String,
    /// HypoPG is installed.
    pub hypopg: bool,
    /// Suggestions may be built in a rolled-back transaction.
    pub allow_ddl: bool,
    /// `y` measures the alternatives (`--measure`).
    pub measure: bool,
}

/// A question waiting for y or n.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub question: String,
    pub ddl: String,
}

/// The search being typed or last confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Search {
    pub query: String,
    pub editing: bool,
}

pub struct App {
    pub plan: Plan,
    pub analysis: Analysis,
    rows: Vec<Row>,
    collapsed: HashSet<NodeId>,
    /// Groups opened by the user, by their first node.
    expanded: HashSet<NodeId>,
    pub selected: usize,
    /// The first tree row on screen.
    pub offset: usize,
    pub focus: Focus,
    pub panel: Panel,
    pub finding: usize,
    /// The selected advice.
    pub advice: usize,
    /// The first advice on screen.
    pub advice_offset: usize,
    pub detail_scroll: u16,
    pub view: View,
    pub search: Option<Search>,
    pub help: bool,
    /// A one-line notice in the status bar.
    pub message: Option<String>,
    /// Set in connected mode.
    pub live: Option<Live>,
    /// A question shown over everything else.
    pub confirm: Option<Confirm>,
    /// Rows of the tree on screen at the last frame, for paging.
    pub tree_height: usize,
    /// The first finding on screen.
    pub findings_offset: usize,
    /// Each node's name, by node index.
    labels: Vec<String>,
    /// Widths of the rows, estimate and buffers columns.
    widths: [usize; 3],
    /// CPU time summed over all nodes.
    cpu_total: f64,
    /// CPU time summed over each node's subtree, by node index.
    subtree_cpu: Vec<f64>,
    /// The icicle view, zoomed on this node, in place of the tree.
    pub icicle: Option<NodeId>,
    /// Columns of the icicle at the last frame, for moving between boxes.
    pub icicle_width: usize,
    /// What the boxes of the icicle are as wide as.
    weights: Weights,
}

/// Keys, independent of the terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Tab,
    Backspace,
}

impl App {
    pub fn new(plan: Plan, analysis: Analysis) -> Self {
        let mut app = App {
            plan,
            analysis,
            rows: Vec::new(),
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            selected: 0,
            offset: 0,
            focus: Focus::Tree,
            panel: Panel::Findings,
            finding: 0,
            advice: 0,
            advice_offset: 0,
            detail_scroll: 0,
            view: View {
                inclusive: false,
                cpu: false,
                buffers: false,
            },
            search: None,
            help: false,
            message: None,
            live: None,
            confirm: None,
            tree_height: 10,
            findings_offset: 0,
            labels: Vec::new(),
            widths: [0; 3],
            cpu_total: 0.0,
            subtree_cpu: Vec::new(),
            icicle: None,
            icicle_width: 80,
            weights: Weights {
                basis: icicle::Basis::Cost,
                own: Vec::new(),
                subtree: Vec::new(),
            },
        };
        app.load();
        app
    }

    /// Shows another plan of the same statement, such as the measured plan
    /// after the estimated one: folds, search and lists start over, and the
    /// selection stays on the same row when it can.
    pub fn replace(&mut self, plan: Plan, analysis: Analysis) {
        let selected = self.selected;
        self.plan = plan;
        self.analysis = analysis;
        self.rows.clear();
        self.collapsed.clear();
        self.expanded.clear();
        self.finding = 0;
        self.findings_offset = 0;
        self.advice = 0;
        self.advice_offset = 0;
        self.detail_scroll = 0;
        self.search = None;
        if self.focus != Focus::Tree {
            self.focus = Focus::Tree;
        }
        self.panel = Panel::Findings;
        // The new plan has other nodes: the icicle starts again at the root.
        if self.icicle.is_some() {
            self.icicle = Some(NodeId(0));
        }
        self.load();
        self.selected = selected.min(self.rows.len().saturating_sub(1));
    }

    /// Derives what the viewer shows from the plan and the analysis.
    fn load(&mut self) {
        let app = self;
        if app.analysis.findings.is_empty() && !app.analysis.advice.is_empty() {
            app.panel = Panel::Advice;
        }
        app.labels = app.plan.nodes.iter().map(format::node).collect();
        let mut widths = ["Rows".len(), "Estimate".len(), 0];
        for node in &app.plan.nodes {
            let counts = crate::ui::counts(app, node.id);
            for (width, cell) in
                widths
                    .iter_mut()
                    .zip([&counts.rows, &counts.estimate, &counts.buffers])
            {
                *width = (*width).max(cell.chars().count());
            }
        }
        if widths[2] > 0 {
            widths[2] = widths[2].max("Buffers".len());
        }
        app.widths = widths;
        // Children before their parents: pre-order, reversed.
        let mut subtree: Vec<f64> = app
            .analysis
            .metrics
            .nodes
            .iter()
            .map(|node| node.exclusive_cpu_time.unwrap_or(0.0))
            .collect();
        for (_, node) in app.plan.walk().into_iter().rev() {
            if let Some(parent) = node.parent {
                subtree[parent.index()] += subtree[node.id.index()];
            }
        }
        app.cpu_total = subtree.first().copied().unwrap_or(0.0);
        app.subtree_cpu = subtree;
        let cpu: Vec<Option<f64>> = app
            .analysis
            .metrics
            .nodes
            .iter()
            .map(|node| node.exclusive_cpu_time)
            .collect();
        app.weights = Weights::new(&app.plan, &cpu);
        app.rebuild();
    }

    /// A node's name: `Seq Scan on orders`.
    pub fn label(&self, id: NodeId) -> &str {
        &self.labels[id.index()]
    }

    pub fn widths(&self) -> [usize; 3] {
        self.widths
    }

    pub fn cpu_total(&self) -> f64 {
        self.cpu_total
    }

    /// CPU time in a node and everything below it.
    pub fn subtree_cpu(&self, id: NodeId) -> f64 {
        self.subtree_cpu[id.index()]
    }

    pub fn weights(&self) -> &Weights {
        &self.weights
    }

    /// The boxes of the icicle across `width` columns. When the selected
    /// node is too narrow to show, the view zooms on its parent.
    pub fn icicle_cells(&mut self, width: usize) -> Vec<Cell> {
        let zoom = self.icicle.unwrap_or(NodeId(0));
        let cells = icicle::layout(&self.plan, &self.weights, zoom, width);
        let selected = self.selected_node();
        if cells.iter().any(|cell| cell.node == selected) {
            return cells;
        }
        let zoom = self.plan.node(selected).parent.unwrap_or(selected);
        self.icicle = Some(zoom);
        icicle::layout(&self.plan, &self.weights, zoom, width)
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn selected_row(&self) -> &Row {
        &self.rows[self.selected]
    }

    pub fn selected_node(&self) -> NodeId {
        self.selected_row().node
    }

    /// Recomputes the visible rows, keeping the selected node selected.
    fn rebuild(&mut self) {
        let keep = self.rows.get(self.selected).map(|row| row.node);
        self.rows = visible_rows(&self.plan, &self.collapsed, &self.expanded);
        self.selected = keep
            .and_then(|node| self.row_of(node))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// The row showing a node, directly or as part of a group.
    fn row_of(&self, node: NodeId) -> Option<usize> {
        self.rows.iter().position(|row| {
            row.node == node
                || row
                    .group
                    .as_ref()
                    .is_some_and(|group| group.contains(&node))
        })
    }

    /// Opens whatever hides a node and selects it.
    pub fn reveal(&mut self, node: NodeId) {
        let mut parent = self.plan.node(node).parent;
        while let Some(id) = parent {
            self.collapsed.remove(&id);
            parent = self.plan.node(id).parent;
        }
        // Open the group the node belongs to, if it is not its first node.
        if let Some(row) = self.row_of_in(node) {
            if let Some(group) = &row.group {
                if group[0] != node {
                    self.expanded.insert(group[0]);
                }
            }
        }
        self.rebuild();
        if let Some(index) = self.row_of(node) {
            self.selected = index;
        }
        self.focus = Focus::Tree;
        self.detail_scroll = 0;
    }

    /// The row a node would be in after opening its ancestors.
    fn row_of_in(&self, node: NodeId) -> Option<Row> {
        visible_rows(&self.plan, &self.collapsed, &self.expanded)
            .into_iter()
            .find(|row| {
                row.node == node
                    || row
                        .group
                        .as_ref()
                        .is_some_and(|group| group.contains(&node))
            })
    }

    /// Keeps the selection on screen for a tree area of `height` rows.
    pub fn scroll_into_view(&mut self, height: usize) {
        let height = height.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(1));
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    pub fn handle(&mut self, key: Key, page: usize) -> Outcome {
        self.message = None;
        if let Some(search) = self.search.as_mut().filter(|search| search.editing) {
            match key {
                Key::Char(c) => search.query.push(c),
                Key::Backspace => {
                    search.query.pop();
                }
                Key::Enter => {
                    search.editing = false;
                    self.find(true, true);
                }
                Key::Esc => self.search = None,
                _ => {}
            }
            return Outcome::Continue;
        }
        if self.help {
            self.help = false;
            return Outcome::Continue;
        }
        if let Some(confirm) = self.confirm.take() {
            if matches!(key, Key::Char('y' | 'Y')) {
                return Outcome::Prove {
                    ddl: confirm.ddl,
                    measured: true,
                };
            }
            self.message = Some("Not built.".to_owned());
            return Outcome::Continue;
        }
        let running = self
            .live
            .as_ref()
            .is_some_and(|live| live.running.is_some());
        match key {
            Key::Esc if running => return Outcome::Cancel,
            Key::Char('q') | Key::Esc => return Outcome::Quit,
            Key::Char('r') | Key::Char('e') if self.live.is_none() => {
                self.message = Some(
                    "Not connected: r and e run the statement in connected mode (explainsql -d … -f query.sql)."
                        .to_owned(),
                );
            }
            Key::Char('r') | Key::Char('e') if running => {
                self.message = Some("A run is in progress; Esc cancels it.".to_owned());
            }
            Key::Char('r') => return Outcome::Run,
            Key::Char('e') => return Outcome::Edit,
            Key::Char('?') => self.help = true,
            Key::Tab => {
                self.focus = match (self.focus, self.panel) {
                    (Focus::Tree, Panel::Findings) if !self.analysis.findings.is_empty() => {
                        Focus::Findings
                    }
                    (Focus::Tree, Panel::Advice) if !self.analysis.advice.is_empty() => {
                        Focus::Advice
                    }
                    _ => Focus::Tree,
                };
            }
            Key::Char('f') => {
                self.panel = Panel::Findings;
                self.focus = if self.focus == Focus::Findings {
                    Focus::Tree
                } else if self.analysis.findings.is_empty() {
                    self.message = Some("No findings for this plan.".to_owned());
                    Focus::Tree
                } else {
                    Focus::Findings
                };
            }
            Key::Char('i') => {
                self.panel = Panel::Advice;
                self.focus = if self.focus == Focus::Advice {
                    Focus::Tree
                } else if self.analysis.advice.is_empty() {
                    self.message = Some("No advice for this plan.".to_owned());
                    Focus::Tree
                } else {
                    Focus::Advice
                };
            }
            Key::Char('c') => return self.copy(),
            Key::Char('t') => return self.prove(),
            Key::Char('y') => return self.why_not(),
            Key::Char('/') => {
                self.search = Some(Search {
                    query: String::new(),
                    editing: true,
                });
            }
            Key::Char('n') => self.find(true, false),
            Key::Char('N') => self.find(false, false),
            Key::Char('x') => self.view.inclusive = !self.view.inclusive,
            Key::Char('w') => self.view.cpu = !self.view.cpu,
            Key::Char('b') => self.view.buffers = !self.view.buffers,
            Key::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(1),
            Key::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            Key::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                match self.analysis.metrics.statement.hotspots.get(index) {
                    Some(&node) => self.reveal(node),
                    None => self.message = Some(format!("There is no hotspot {digit}.")),
                }
            }
            Key::Char('F') => {
                self.icicle = match self.icicle {
                    Some(_) => None,
                    None => Some(NodeId(0)),
                };
                self.focus = Focus::Tree;
            }
            _ if self.focus == Focus::Findings => self.findings_key(key, page),
            _ if self.focus == Focus::Advice => self.advice_key(key, page),
            _ if self.icicle.is_some() => self.icicle_key(key),
            _ => self.tree_key(key, page),
        }
        Outcome::Continue
    }

    fn tree_key(&mut self, key: Key, page: usize) {
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Down | Key::Char('j') => self.select(self.selected + 1),
            Key::Up | Key::Char('k') => self.select(self.selected.saturating_sub(1)),
            Key::PageDown => self.select(self.selected + page.max(1)),
            Key::PageUp => self.select(self.selected.saturating_sub(page.max(1))),
            Key::Home | Key::Char('g') => self.select(0),
            Key::End | Key::Char('G') => self.select(last),
            Key::Left | Key::Char('h') => self.close_or_parent(),
            Key::Right | Key::Char('l') => self.open(),
            Key::Enter | Key::Char(' ') => {
                let row = self.selected_row().clone();
                if row.collapsed || row.group.is_some() {
                    self.open();
                } else if row.has_children {
                    self.close_or_parent();
                }
            }
            _ => {}
        }
    }

    /// Moves between the boxes of the icicle: k to the parent, j to the
    /// widest child, h and l along the row; Enter zooms on a box, or out of
    /// the box zoomed on.
    fn icicle_key(&mut self, key: Key) {
        let cells = self.icicle_cells(self.icicle_width);
        let zoom = self.icicle.unwrap_or(NodeId(0));
        let selected = self.selected_node();
        let Some(here) = cells.iter().find(|cell| cell.node == selected).cloned() else {
            return;
        };
        let row = |depth: usize| {
            let mut row: Vec<&Cell> = cells.iter().filter(|cell| cell.depth == depth).collect();
            row.sort_by_key(|cell| cell.x);
            row
        };
        let target = match key {
            Key::Up | Key::Char('k') => {
                if selected == zoom {
                    // Above the box zoomed on: zoom out.
                    let parent = self.plan.node(zoom).parent;
                    if parent.is_some() {
                        self.icicle = parent;
                    }
                    parent
                } else {
                    self.plan.node(selected).parent
                }
            }
            Key::Down | Key::Char('j') => {
                let child = cells
                    .iter()
                    .filter(|cell| self.plan.node(cell.node).parent == Some(selected))
                    .max_by_key(|cell| cell.width)
                    .map(|cell| cell.node);
                if child.is_none() {
                    self.message = Some(match here.folded {
                        0 => "This node has nothing below it.".to_owned(),
                        n => format!(
                            "{n} node{} below are too narrow to show; Enter zooms on this one.",
                            if n == 1 { "" } else { "s" }
                        ),
                    });
                }
                child
            }
            Key::Left | Key::Char('h') => row(here.depth)
                .iter()
                .rev()
                .find(|cell| cell.x < here.x)
                .map(|cell| cell.node),
            Key::Right | Key::Char('l') => row(here.depth)
                .iter()
                .find(|cell| cell.x > here.x)
                .map(|cell| cell.node),
            Key::Enter | Key::Char(' ') => {
                if selected == zoom {
                    if let Some(parent) = self.plan.node(zoom).parent {
                        self.icicle = Some(parent);
                    }
                } else {
                    self.icicle = Some(selected);
                }
                None
            }
            Key::Home | Key::Char('g') => {
                self.icicle = Some(NodeId(0));
                Some(NodeId(0))
            }
            _ => None,
        };
        if let Some(node) = target {
            self.reveal(node);
        }
    }

    fn findings_key(&mut self, key: Key, page: usize) {
        if list_key(key, page, self.analysis.findings.len(), &mut self.finding) {
            self.detail_scroll = 0;
            return;
        }
        match key {
            Key::Enter | Key::Right | Key::Char('l') => {
                match self
                    .analysis
                    .findings
                    .get(self.finding)
                    .and_then(|finding| finding.node)
                {
                    Some(node) => self.reveal(node),
                    None => {
                        self.message = Some("This finding is about the whole statement.".to_owned())
                    }
                }
            }
            _ => {}
        }
    }

    fn advice_key(&mut self, key: Key, page: usize) {
        if list_key(key, page, self.analysis.advice.len(), &mut self.advice) {
            self.detail_scroll = 0;
            return;
        }
        if matches!(key, Key::Enter | Key::Right | Key::Char('l')) {
            match self
                .analysis
                .advice
                .get(self.advice)
                .and_then(|advice| advice.node)
            {
                Some(node) => self.reveal(node),
                None => {
                    self.message = Some("This advice is about the statement as a whole.".to_owned())
                }
            }
        }
    }

    /// The `CREATE INDEX` statement of the selected advice, or of the advice
    /// for the selected node, for the clipboard.
    fn copy(&mut self) -> Outcome {
        let advice = match self.focus {
            Focus::Advice => self.analysis.advice.get(self.advice),
            _ => {
                let node = self.selected_node();
                self.analysis
                    .advice
                    .iter()
                    .find(|advice| advice.node == Some(node) && advice.index().is_some())
            }
        };
        match advice.map(|advice| &advice.kind) {
            Some(AdviceKind::Index { ddl, .. }) => {
                self.message = Some(format!("Copied: {ddl}"));
                Outcome::Copy(ddl.clone())
            }
            _ => {
                self.message = Some(
                    "Nothing to copy: select an index suggestion (press i for the advice)."
                        .to_owned(),
                );
                Outcome::Continue
            }
        }
    }

    /// The index suggestion in focus: the selected advice, or the advice
    /// for the selected node.
    fn index_advice(&self) -> Option<&explainsql_core::advisor::Advice> {
        match self.focus {
            Focus::Advice => self
                .analysis
                .advice
                .get(self.advice)
                .filter(|advice| advice.index().is_some()),
            _ => {
                let node = self.selected_node();
                self.analysis
                    .advice
                    .iter()
                    .find(|advice| advice.node == Some(node) && advice.index().is_some())
            }
        }
    }

    /// Tests the index suggestion in focus: with HypoPG at once, or after
    /// asking, by building it in a rolled-back transaction.
    fn prove(&mut self) -> Outcome {
        let Some(live) = &self.live else {
            self.message = Some(
                "Not connected: t tests a suggestion in connected mode (explainsql -d … -f query.sql)."
                    .to_owned(),
            );
            return Outcome::Continue;
        };
        if live.running.is_some() {
            self.message = Some("A run is in progress; Esc cancels it.".to_owned());
            return Outcome::Continue;
        }
        let (hypopg, allow_ddl) = (live.hypopg, live.allow_ddl);
        let Some(advice) = self.index_advice() else {
            self.message =
                Some("Select an index suggestion to test (press i for the advice).".to_owned());
            return Outcome::Continue;
        };
        let Some(AdviceKind::Index { ddl, index }) = Some(&advice.kind) else {
            return Outcome::Continue;
        };
        if hypopg {
            return Outcome::Prove {
                ddl: ddl.clone(),
                measured: false,
            };
        }
        if !allow_ddl {
            self.message = Some(
                "To test it, install HypoPG (CREATE EXTENSION hypopg), or restart with --allow-ddl to build it in a rolled-back transaction."
                    .to_owned(),
            );
            return Outcome::Continue;
        }
        let size = advice
            .caveats
            .iter()
            .find_map(|caveat| caveat.strip_prefix("Building it reads all of "))
            .and_then(|rest| rest.split_once(" with its indexes"))
            .and_then(|(table, _)| table.split_once(" ("))
            .map(|(_, size)| format!(" ({size})"))
            .unwrap_or_default();
        self.confirm = Some(Confirm {
            question: format!(
                "Build the index on {}{size} inside a transaction that is rolled back? Writes to {} wait until it is built. y/n",
                index.table, index.table
            ),
            ddl: ddl.clone(),
        });
        Outcome::Continue
    }

    /// Asks the planner why it chose what it chose for the selected node:
    /// a sequential scan rather than an index, a nested loop, or, when
    /// measuring, a spill to disk.
    fn why_not(&mut self) -> Outcome {
        let Some(live) = &self.live else {
            self.message = Some(
                "Not connected: y asks the database why the planner chose this node (explainsql -d … -f query.sql)."
                    .to_owned(),
            );
            return Outcome::Continue;
        };
        if live.running.is_some() {
            self.message = Some("A run is in progress; Esc cancels it.".to_owned());
            return Outcome::Continue;
        }
        let node = self.selected_node();
        let asked = explainsql_core::counterfactual::questions(
            &self.plan,
            &self.analysis,
            None,
            &explainsql_core::counterfactual::Target::Node(node),
            live.measure,
        );
        if asked.is_empty() {
            self.message = Some(format!(
                "Nothing to ask about {}: y works on sequential scans and nested loops{}.",
                self.label(node),
                if live.measure {
                    ", and on sorts and hashes that spilled to disk"
                } else {
                    "; with --measure, also on sorts and hashes that spilled to disk"
                }
            ));
            return Outcome::Continue;
        }
        Outcome::WhyNot { node }
    }

    /// Opens a collapsed node or a group of siblings.
    fn open(&mut self) {
        let row = self.selected_row().clone();
        if let Some(group) = &row.group {
            self.expanded.insert(group[0]);
        } else if row.collapsed {
            self.collapsed.remove(&row.node);
        } else {
            return;
        }
        self.rebuild();
    }

    /// Collapses an open node; on a closed node or a leaf, goes to the
    /// parent.
    fn close_or_parent(&mut self) {
        let row = self.selected_row().clone();
        if row.has_children && !row.collapsed && row.group.is_none() {
            self.collapsed.insert(row.node);
            self.rebuild();
            return;
        }
        // Closing a group that was opened: regroup it.
        let node = row.node;
        if let Some(first) = self.expanded.iter().copied().find(|&first| {
            self.plan.node(first).parent == self.plan.node(node).parent && first <= node
        }) {
            if self.expanded.remove(&first) {
                self.rebuild();
                if let Some(index) = self.row_of(first) {
                    self.selected = index;
                }
                return;
            }
        }
        if let Some(parent) = self.plan.node(node).parent {
            if let Some(index) = self.row_of(parent) {
                self.select(index);
            }
        }
    }

    /// Nodes whose name or conditions contain the query, in plan order.
    pub fn matches(&self) -> Vec<NodeId> {
        let Some(query) = self
            .search
            .as_ref()
            .map(|search| search.query.to_lowercase())
        else {
            return Vec::new();
        };
        if query.is_empty() {
            return Vec::new();
        }
        self.plan
            .walk()
            .into_iter()
            .map(|(_, node)| node)
            .filter(|node| {
                self.labels[node.id.index()].to_lowercase().contains(&query)
                    || node
                        .predicates
                        .iter()
                        .any(|predicate| predicate.text.to_lowercase().contains(&query))
            })
            .map(|node| node.id)
            .collect()
    }

    /// Moves to the next (or previous) match; `inclusive` accepts the
    /// selected node itself.
    fn find(&mut self, forward: bool, inclusive: bool) {
        let matches = self.matches();
        if matches.is_empty() {
            if self.search.is_some() {
                self.message = Some("No match.".to_owned());
            }
            return;
        }
        let order: Vec<NodeId> = self
            .plan
            .walk()
            .into_iter()
            .map(|(_, node)| node.id)
            .collect();
        let position = |id: NodeId| order.iter().position(|&other| other == id).unwrap_or(0);
        let current = position(self.selected_node());
        let next = if forward {
            matches
                .iter()
                .copied()
                .find(|&id| position(id) > current || (inclusive && position(id) == current))
                .unwrap_or(matches[0])
        } else {
            matches
                .iter()
                .rev()
                .copied()
                .find(|&id| position(id) < current)
                .unwrap_or(*matches.last().expect("not empty"))
        };
        self.reveal(next);
        let index = matches.iter().position(|&id| id == next).unwrap_or(0);
        self.message = Some(format!("Match {} of {}", index + 1, matches.len()));
    }
}

/// Moves the selection of a list; `false` when the key is not a move.
fn list_key(key: Key, page: usize, len: usize, index: &mut usize) -> bool {
    let last = len.saturating_sub(1);
    *index = match key {
        Key::Down | Key::Char('j') => (*index + 1).min(last),
        Key::Up | Key::Char('k') => index.saturating_sub(1),
        Key::PageDown => (*index + page.max(1)).min(last),
        Key::PageUp => index.saturating_sub(page.max(1)),
        Key::Home | Key::Char('g') => 0,
        Key::End | Key::Char('G') => last,
        _ => return false,
    };
    true
}

/// The rows to show: the tree in plan order, without the descendants of
/// collapsed nodes, with runs of similar siblings folded into one row
/// unless they were opened.
pub fn visible_rows(
    plan: &Plan,
    collapsed: &HashSet<NodeId>,
    expanded: &HashSet<NodeId>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    // (node or group, own prefix, prefix for children)
    let mut stack: Vec<(Vec<NodeId>, String, String)> =
        vec![(vec![NodeId(0)], String::new(), String::new())];
    while let Some((nodes, own, below)) = stack.pop() {
        let node = plan.node(nodes[0]);
        let has_children = !node.children.is_empty();
        let is_collapsed = collapsed.contains(&node.id);
        let group = (nodes.len() > 1).then(|| nodes.clone());
        rows.push(Row {
            node: node.id,
            prefix: own,
            group: group.clone(),
            has_children: has_children && group.is_none(),
            collapsed: is_collapsed && group.is_none(),
        });
        if group.is_some() || is_collapsed {
            continue;
        }
        let items = sibling_runs(plan, &node.children, expanded);
        for (index, item) in items.iter().enumerate().rev() {
            let last = index + 1 == items.len();
            stack.push((
                item.clone(),
                format!("{below}{}", if last { "└─ " } else { "├─ " }),
                format!("{below}{}", if last { "   " } else { "│  " }),
            ));
        }
    }
    rows
}

/// Children split into single nodes and runs of similar leaves.
fn sibling_runs(plan: &Plan, children: &[NodeId], expanded: &HashSet<NodeId>) -> Vec<Vec<NodeId>> {
    let mut items: Vec<Vec<NodeId>> = Vec::new();
    let mut index = 0;
    while index < children.len() {
        let key = leaf_key(plan, children[index]);
        let mut end = index + 1;
        while key.is_some() && end < children.len() && leaf_key(plan, children[end]) == key {
            end += 1;
        }
        let run = &children[index..end];
        if run.len() >= MIN_GROUP && !expanded.contains(&run[0]) {
            items.push(run.to_vec());
        } else {
            items.extend(run.iter().map(|&id| vec![id]));
        }
        index = end;
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(text: &str) -> App {
        let plan = explainsql_core::parse(text).unwrap();
        let analysis = explainsql_core::analyze(&plan);
        App::new(plan, analysis)
    }

    const JOIN: &str = "\
Hash Join  (cost=1.00..10.00 rows=10 width=8) (actual time=0.100..5.000 rows=10 loops=1)
  Hash Cond: (a.id = b.id)
  ->  Seq Scan on a  (cost=0.00..5.00 rows=100 width=4) (actual time=0.010..2.500 rows=100 loops=1)
        Filter: (x = 1)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=1.000..1.000 rows=10 loops=1)
        ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.500 rows=10 loops=1)
Execution Time: 5.500 ms";

    fn partitions(count: usize) -> String {
        let mut text = String::from(
            "Append  (cost=0.00..10.00 rows=10 width=4) (actual time=0.010..9.000 rows=10 loops=1)\n",
        );
        for n in 1..=count {
            text.push_str(&format!(
                "  ->  Seq Scan on events_2025_{n:02} events_{n}  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.500 rows=1 loops=1)\n        Filter: (events_{n}.payload @> '{{}}'::jsonb)\n"
            ));
        }
        text
    }

    fn labels(app: &App) -> Vec<String> {
        app.rows()
            .iter()
            .map(|row| format!("{}{}", row.prefix, format::node(app.plan.node(row.node))))
            .collect()
    }

    #[test]
    fn moves_folds_and_unfolds() {
        let mut app = app(JOIN);
        assert_eq!(
            labels(&app),
            [
                "Hash Join",
                "├─ Seq Scan on a",
                "└─ Hash",
                "   └─ Seq Scan on b"
            ]
        );
        app.handle(Key::Char('G'), 10);
        assert_eq!(app.selected, 3);
        // On a leaf, h goes to the parent; on an open node, it closes it.
        app.handle(Key::Char('h'), 10);
        assert_eq!(app.selected_node(), NodeId(2));
        app.handle(Key::Char('h'), 10);
        assert_eq!(app.rows().len(), 3);
        assert!(app.selected_row().collapsed);
        app.handle(Key::Char('l'), 10);
        assert_eq!(app.rows().len(), 4);
        assert_eq!(app.handle(Key::Char('q'), 10), Outcome::Quit);
    }

    #[test]
    fn groups_similar_siblings() {
        let mut app = app(&partitions(12));
        assert_eq!(app.rows().len(), 2);
        assert_eq!(app.rows()[1].group.as_ref().map(Vec::len), Some(12));
        app.handle(Key::Char('j'), 10);
        app.handle(Key::Enter, 10);
        assert_eq!(app.rows().len(), 13);
        // h on a member of an opened group folds it back.
        app.handle(Key::Char('j'), 10);
        app.handle(Key::Char('h'), 10);
        assert_eq!(app.rows().len(), 2);
        // A few siblings are not grouped.
        assert_eq!(
            super::App::new(
                explainsql_core::parse(&partitions(3)).unwrap(),
                explainsql_core::analyze(&explainsql_core::parse(&partitions(3)).unwrap()),
            )
            .rows()
            .len(),
            4
        );
    }

    #[test]
    fn searches_and_jumps() {
        let mut app = app(JOIN);
        app.handle(Key::Char('h'), 10); // collapse the root? root stays selected
        for key in [
            Key::Char('/'),
            Key::Char('o'),
            Key::Char('n'),
            Key::Char(' '),
            Key::Char('b'),
            Key::Enter,
        ] {
            app.handle(key, 10);
        }
        // "on b" is hidden under the collapsed root and gets revealed.
        assert_eq!(app.selected_node(), NodeId(3));
        assert_eq!(app.message.as_deref(), Some("Match 1 of 1"));

        // Hotspots: the scan of a takes the most time.
        app.handle(Key::Char('1'), 10);
        assert_eq!(app.selected_node(), NodeId(1));
        app.handle(Key::Char('7'), 10);
        assert_eq!(app.message.as_deref(), Some("There is no hotspot 7."));
    }

    #[test]
    fn goes_from_a_finding_to_its_node() {
        let plan = explainsql_core::parse(
            "\
Limit  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..12.000 rows=1 loops=1)
  ->  Sort  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..12.000 rows=1 loops=1)
        Sort Key: a
        ->  Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
              Filter: (customer_id = 4242)
              Rows Removed by Filter: 199990
              Buffers: shared hit=2031 read=386
Execution Time: 12.100 ms",
        )
        .unwrap();
        let analysis = explainsql_core::analyze(&plan);
        let mut app = App::new(plan, analysis);
        app.handle(Key::Char('h'), 10);
        app.handle(Key::Tab, 10);
        assert_eq!(app.focus, Focus::Findings);
        app.handle(Key::Enter, 10);
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(app.selected_node(), NodeId(2));
    }

    #[test]
    fn keeps_the_selection_on_screen() {
        let mut app = app(&partitions(3));
        app.handle(Key::Char('G'), 2);
        app.scroll_into_view(2);
        assert_eq!(app.offset, 2);
        app.handle(Key::Char('g'), 2);
        app.scroll_into_view(2);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn tests_a_suggestion_after_asking() {
        let plan = explainsql_core::parse(
            "\
Seq Scan on public.orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.053..11.865 rows=10 loops=1)
  Filter: (orders.customer_id = 4242)
  Rows Removed by Filter: 199990
  Buffers: shared hit=2031 read=386
Execution Time: 11.900 ms",
        )
        .unwrap();
        let analysis = explainsql_core::analyze(&plan);
        let mut app = App::new(plan, analysis);
        // Offline, t explains how to test.
        assert_eq!(app.handle(Key::Char('t'), 10), Outcome::Continue);
        assert!(app.message.as_deref().unwrap().starts_with("Not connected"));
        app.live = Some(Live {
            database: "db".to_owned(),
            sql: "SELECT".to_owned(),
            measured: true,
            running: None,
            task: String::new(),
            hypopg: false,
            allow_ddl: true,
            measure: false,
        });
        // Building the index asks first; anything but y declines.
        assert_eq!(app.handle(Key::Char('t'), 10), Outcome::Continue);
        assert!(app.confirm.is_some());
        assert_eq!(app.handle(Key::Char('n'), 10), Outcome::Continue);
        assert!(app.confirm.is_none());
        app.handle(Key::Char('t'), 10);
        assert_eq!(
            app.handle(Key::Char('y'), 10),
            Outcome::Prove {
                ddl: "CREATE INDEX CONCURRENTLY ON public.orders (customer_id);".to_owned(),
                measured: true
            }
        );
        // With HypoPG, nothing is built and nothing is asked.
        if let Some(live) = &mut app.live {
            live.hypopg = true;
        }
        assert!(matches!(
            app.handle(Key::Char('t'), 10),
            Outcome::Prove {
                measured: false,
                ..
            }
        ));
    }

    #[test]
    fn asks_the_planner_why() {
        let plan = explainsql_core::parse(
            "\
Nested Loop  (cost=0.29..20.00 rows=10 width=8) (actual time=0.020..5.000 rows=10 loops=1)
  ->  Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=4) (actual time=0.010..4.000 rows=10 loops=1)
        Filter: (customer_id = 4242)
        Rows Removed by Filter: 199990
  ->  Index Scan using customers_pkey on customers c  (cost=0.29..1.00 rows=1 width=4) (actual time=0.010..0.010 rows=1 loops=10)
        Index Cond: (id = o.customer_id)
Execution Time: 5.100 ms",
        )
        .unwrap();
        let analysis = explainsql_core::analyze(&plan);
        let mut app = App::new(plan, analysis);
        app.handle(Key::Char('j'), 10);
        // Offline, y explains what it needs.
        assert_eq!(app.handle(Key::Char('y'), 10), Outcome::Continue);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .starts_with("Not connected: y asks the database")
        );
        app.live = Some(Live {
            database: "db".to_owned(),
            sql: "SELECT".to_owned(),
            measured: true,
            running: None,
            task: String::new(),
            hypopg: false,
            allow_ddl: false,
            measure: false,
        });
        assert_eq!(
            app.handle(Key::Char('y'), 10),
            Outcome::WhyNot { node: NodeId(1) }
        );
        // An index scan already uses an index: nothing to ask.
        app.handle(Key::Char('j'), 10);
        assert_eq!(app.handle(Key::Char('y'), 10), Outcome::Continue);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .starts_with("Nothing to ask about Index Scan using customers_pkey"),
            "{:?}",
            app.message
        );
        // One question at a time.
        app.handle(Key::Char('k'), 10);
        if let Some(live) = &mut app.live {
            live.running = Some(std::time::Instant::now());
        }
        assert_eq!(app.handle(Key::Char('y'), 10), Outcome::Continue);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .starts_with("A run is in progress")
        );
    }
}
