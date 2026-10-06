//! `EXPLAIN` text format into the raw tree, under the property names of the
//! JSON format.
//!
//! The text format is laid out by indentation (see `ExplainNode` in
//! PostgreSQL's `explain.c`): a node's properties start two columns after
//! its name; a child starts with `->  ` at its parent's property column; an
//! InitPlan or SubPlan child is announced by a label line at that column,
//! with its arrow two columns further in; and lines at column 0 after the
//! root describe the whole statement (`Planning Time`, triggers, `JIT`, ...).
//! Values the text format leaves out because they are zero or false are not
//! invented here; lowering treats both formats the same way.

use serde_json::{Map, Value, json};

use super::raw::{RawPlan, number};
use crate::ir::Warning;

/// The first plan in `text`. Another plan after it, such as an "after" plan
/// pasted below the "before" one, is left out with a warning.
pub(crate) fn parse(text: &str, warnings: &mut Vec<Warning>) -> Option<RawPlan> {
    let lines: Vec<&str> = text.lines().collect();
    let ((plan, _), next) = parse_at(&lines, 0, warnings)?;
    if let Some(next) = next {
        warnings.push(Warning {
            line: Some(next + 1),
            message: "the input contains more than one plan; showing the first".to_owned(),
        });
    }
    Some(plan)
}

/// Every plan in `text`, one after the other, each with its warnings. Line
/// numbers count from the start of `text`. Lines between two plans that
/// belong to neither, such as `After:`, are left out with a warning on the
/// plan they precede.
pub(crate) fn parse_all(text: &str) -> Vec<(RawPlan, Vec<Warning>)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut plans = Vec::new();
    let mut start = 0;
    let mut skipped = 0;
    loop {
        let mut warnings = Vec::new();
        if skipped > 0 {
            warnings.push(Warning {
                line: None,
                message: format!("ignored {skipped} line(s) before the plan"),
            });
        }
        let Some((plan, next)) = parse_at(&lines, start, &mut warnings) else {
            break;
        };
        plans.push((plan.0, warnings));
        skipped = plan.1;
        match next {
            Some(next) => start = next,
            None => break,
        }
    }
    plans
}

/// The plan whose root is the first line from `start` on, and the index of
/// the line where the next plan starts, if one does. With the plan comes
/// the number of lines just before the next plan that belonged to neither
/// and were taken back from this one.
fn parse_at(
    lines: &[&str],
    start: usize,
    warnings: &mut Vec<Warning>,
) -> Option<((RawPlan, usize), Option<usize>)> {
    let mut parser = Parser {
        plan: RawPlan::new(),
        warnings,
        stack: Vec::new(),
        label: None,
        labeled: Vec::new(),
        worker: None,
        section: None,
        in_summary: false,
        next: None,
        unread: 0,
    };
    for (index, line) in lines.iter().enumerate().skip(start) {
        if line.trim().is_empty() {
            continue;
        }
        let unparsed = parser.unparsed_count();
        parser.line(index + 1, line);
        if parser.plan.nodes.is_empty() {
            // The first line must be the root node.
            return None;
        }
        if parser.next.is_some() {
            break;
        }
        // Statement-level lines the parser could not read, in a row.
        let unread =
            parser.in_summary && !line.starts_with(' ') && parser.unparsed_count() > unparsed;
        parser.unread = if unread { parser.unread + 1 } else { 0 };
    }
    let next = parser.next.map(|number| number - 1);
    // Text just before another plan, such as `After:`, introduces it.
    let taken = if next.is_some() {
        parser.take_back_unread()
    } else {
        0
    };
    Some(((parser.finish(), taken), next))
}

/// How many times one property may repeat on a node before further copies
/// are dropped.
const MAX_REPEATS: usize = 64;

/// Where a property line goes.
#[derive(Clone, Copy)]
enum Target {
    Node(usize),
    /// A worker of a node: (node, index in its `Workers` array).
    Worker(usize, usize),
    /// A statement-level section: `Planning`, `JIT` or `Serialization`.
    Section(&'static str),
}

struct Parser<'w> {
    plan: RawPlan,
    warnings: &'w mut Vec<Warning>,
    /// Open nodes, innermost last: (node, column of its properties).
    stack: Vec<(usize, usize)>,
    /// An InitPlan, SubPlan or CTE label waiting for its node:
    /// (column, subplan name, relationship).
    label: Option<(usize, String, &'static str)>,
    /// Whether a node's relationship came from a label.
    labeled: Vec<bool>,
    /// The worker block being read: (node, worker index, column of its lines).
    worker: Option<(usize, usize, usize)>,
    /// The statement-level section being read.
    section: Option<&'static str>,
    /// Whether the statement-level lines after the tree have started.
    in_summary: bool,
    /// The line where another plan starts, which ends this one.
    next: Option<usize>,
    /// How many statement-level lines in a row the parser could not read,
    /// up to the current line.
    unread: usize,
}

impl Parser<'_> {
    fn line(&mut self, number: usize, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let column = line.len() - line.trim_start_matches(' ').len();
        let content = line[column..].trim_end();

        if self.plan.nodes.is_empty() {
            self.root(number, column, content);
        } else if let Some(header) = content.strip_prefix("->") {
            self.child(number, column, header);
        } else if column == 0 {
            if header_line(content, &mut Map::new()) {
                // A second plan, such as an "after" plan pasted below the
                // "before" one. Its summary must not overwrite this one's.
                self.next = Some(number);
                return;
            }
            self.statement_line(number, content);
        } else if self.in_summary {
            match self.section {
                Some(section) => self.property(number, content, Target::Section(section)),
                None => self.unparsed(number, content, None),
            }
        } else if let Some((name, relationship)) = subplan_label(content) {
            self.label = Some((column, name, relationship));
        } else {
            self.node_property(number, column, content);
        }
    }

    fn root(&mut self, number: usize, column: usize, content: &str) {
        let (header, name_column) = match content.strip_prefix("->") {
            Some(rest) => {
                let header = rest.trim_start();
                (header, column + content.len() - header.len())
            }
            None => (content, column),
        };
        let mut props = Map::new();
        if !header_line(header, &mut props) {
            return;
        }
        let index = self.plan.add_node(None, Some(number));
        self.plan.nodes[index].props = props;
        self.labeled.push(false);
        self.stack.push((index, name_column + 2));
    }

    fn child(&mut self, number: usize, column: usize, rest: &str) {
        let header = rest.trim_start();
        let name_column = column + 2 + rest.len() - header.len();
        if self.in_summary {
            self.warn(number, "a plan node after the statement summary");
            self.in_summary = false;
        }
        self.worker = None;
        self.section = None;
        self.close_nodes_deeper_than(column);
        let (parent, parent_column) = *self.stack.last().unwrap_or(&(0, 0));
        let label = self.label.take();
        if label.is_none() && parent_column != column {
            self.warn(number, "unexpected indentation");
        }

        let index = self.plan.add_node(Some(parent), Some(number));
        let mut props = Map::new();
        if !header_line(header, &mut props) {
            self.warn(number, &format!("unknown node type `{header}`"));
        }
        if let Some((_, name, relationship)) = label {
            props.insert("Parent Relationship".to_owned(), json!(relationship));
            props.insert("Subplan Name".to_owned(), json!(name));
        }
        self.labeled.push(label_was_used(&props));
        self.plan.nodes[index].props = props;
        self.stack.push((index, name_column + 2));
    }

    fn close_nodes_deeper_than(&mut self, column: usize) {
        while self.stack.len() > 1 && self.stack.last().is_some_and(|&(_, c)| c > column) {
            self.stack.pop();
        }
    }

    fn node_property(&mut self, number: usize, column: usize, content: &str) {
        if let Some((node, worker, worker_column)) = self.worker {
            if column >= worker_column {
                self.property(number, content, Target::Worker(node, worker));
                return;
            }
            self.worker = None;
        }
        self.close_nodes_deeper_than(column);
        let &(node, node_column) = self.stack.last().expect("the root is always open");
        if column != node_column {
            self.warn(number, "unexpected indentation");
        }
        if let Some((worker_number, rest)) = worker_line(content) {
            let workers = self.plan.nodes[node]
                .props
                .entry("Workers")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(workers) = workers {
                workers.push(json!({ "Worker Number": worker_number }));
                let worker = workers.len() - 1;
                self.worker = Some((node, worker, column + 2));
                if !rest.is_empty() {
                    self.property(number, rest, Target::Worker(node, worker));
                }
                return;
            }
        }
        self.property(number, content, Target::Node(node));
    }

    fn statement_line(&mut self, number: usize, content: &str) {
        self.in_summary = true;
        self.worker = None;
        self.section = None;
        if content.starts_with("Trigger ") || content.starts_with("Trigger:") {
            match trigger(content) {
                Some(trigger) => {
                    let triggers = self
                        .plan
                        .top
                        .entry("Triggers")
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(triggers) = triggers {
                        triggers.push(Value::Object(trigger));
                    }
                }
                None => self.unparsed(number, content, None),
            }
            return;
        }
        let Some((label, value)) = split_label(content) else {
            self.unparsed(number, content, None);
            return;
        };
        let parsed = match label {
            "Planning Time" | "Execution Time" | "Total runtime" => {
                let key = if label == "Total runtime" {
                    "Execution Time"
                } else {
                    label
                };
                milliseconds(value).map(|ms| self.set_top(key, number_value(ms)))
            }
            "Settings" => {
                settings(value).map(|settings| self.set_top(label, Value::Object(settings)))
            }
            "Planning" | "JIT" if value.is_empty() => {
                let section = if label == "JIT" { "JIT" } else { "Planning" };
                self.set_top(section, Value::Object(Map::new()));
                self.section = Some(section);
                Some(())
            }
            "Serialization" => serialization(value).map(|serialization| {
                self.set_top("Serialization", Value::Object(serialization));
                self.section = Some("Serialization");
            }),
            "Query Identifier" => value
                .parse::<i64>()
                .ok()
                .map(|id| self.set_top(label, json!(id))),
            "Query Text" => {
                self.set_top(label, json!(value));
                Some(())
            }
            _ => None,
        };
        if parsed.is_none() {
            self.unparsed(number, content, None);
        }
    }

    fn set_top(&mut self, key: &str, value: Value) {
        self.plan.top.insert(key.to_owned(), value);
    }

    /// Reads one `Label: value` line into the given target.
    fn property(&mut self, number: usize, content: &str, target: Target) {
        let mut props = Map::new();
        let parsed = match target {
            Target::Section("JIT") => read_jit_property(content, &mut props),
            Target::Section(_) => read_section_property(content, &mut props),
            Target::Node(_) | Target::Worker(..) => read_property(content, &mut props),
        };
        match parsed {
            Parsed::Known => {}
            Parsed::Unknown => self.warn(
                number,
                &format!(
                    "unrecognized property `{}`",
                    split_label(content).map_or(content, |(label, _)| label)
                ),
            ),
            Parsed::Invalid => {
                self.unparsed(number, content, Some(target));
                return;
            }
        }
        let destination = self.target(target);
        let mut dropped = false;
        for (key, value) in props {
            if !destination.contains_key(&key) {
                destination.insert(key, value);
                continue;
            }
            // A repeated property (grouping sets print several keys); the
            // limit keeps absurd input from taking quadratic time.
            match (2..=MAX_REPEATS).find(|n| !destination.contains_key(&format!("{key} ({n})"))) {
                Some(n) => {
                    destination.insert(format!("{key} ({n})"), value);
                }
                None => dropped = true,
            }
        }
        if dropped {
            self.warn(number, "ignored a property repeated too many times");
        }
    }

    fn target(&mut self, target: Target) -> &mut Map<String, Value> {
        match target {
            Target::Node(node) => &mut self.plan.nodes[node].props,
            Target::Worker(node, worker) => self.plan.nodes[node]
                .props
                .get_mut("Workers")
                .and_then(Value::as_array_mut)
                .and_then(|workers| workers.get_mut(worker))
                .and_then(Value::as_object_mut)
                .expect("worker blocks are created before their lines"),
            Target::Section(section) => {
                let value = self
                    .plan
                    .top
                    .entry(section)
                    .or_insert_with(|| Value::Object(Map::new()));
                if !value.is_object() {
                    *value = Value::Object(Map::new());
                }
                value.as_object_mut().expect("just made an object")
            }
        }
    }

    /// Keeps a line that could not be read, so that nothing is lost.
    /// How many statement-level lines could not be read so far.
    fn unparsed_count(&self) -> usize {
        match self.plan.top.get("Unparsed Lines") {
            Some(Value::Array(lines)) => lines.len(),
            _ => 0,
        }
    }

    /// Takes the last unread statement-level lines back, with their
    /// warnings, and returns how many there were.
    fn take_back_unread(&mut self) -> usize {
        let count = self.unread;
        if count == 0 {
            return 0;
        }
        if let Some(Value::Array(lines)) = self.plan.top.get_mut("Unparsed Lines") {
            lines.truncate(lines.len().saturating_sub(count));
            if lines.is_empty() {
                self.plan.top.remove("Unparsed Lines");
            }
        }
        // Their warnings are the last ones: nothing after them was read.
        let keep = self.warnings.len().saturating_sub(count);
        self.warnings.truncate(keep);
        count
    }

    fn unparsed(&mut self, number: usize, content: &str, target: Option<Target>) {
        self.warn(number, &format!("could not read `{content}`"));
        let lines = match target {
            Some(target) => self.target(target),
            None => &mut self.plan.top,
        }
        .entry("Unparsed Lines")
        .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(lines) = lines {
            lines.push(json!(content));
        }
    }

    fn warn(&mut self, line: usize, message: &str) {
        self.warnings.push(Warning {
            line: Some(line),
            message: message.to_owned(),
        });
    }

    /// Sets the relationship of children that had no label: positional for
    /// most nodes, `Member` for the children of appends and bitmap
    /// combinations, `Subquery` under a subquery scan.
    fn finish(mut self) -> RawPlan {
        for parent in 0..self.plan.nodes.len() {
            let node_type = self.plan.nodes[parent]
                .props
                .get("Node Type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let ordinary: Vec<usize> = self.plan.nodes[parent]
                .children
                .iter()
                .copied()
                .filter(|&child| !self.labeled[child])
                .collect();
            for (position, &child) in ordinary.iter().enumerate() {
                let relationship = match node_type.as_str() {
                    "Append" | "Merge Append" | "BitmapAnd" | "BitmapOr" | "Custom Scan" => {
                        "Member"
                    }
                    "Subquery Scan" => "Subquery",
                    "ModifyTable" if ordinary.len() > 1 => "Member",
                    _ if position == 0 => "Outer",
                    _ if position == 1 => "Inner",
                    _ => "Member",
                };
                self.plan.nodes[child]
                    .props
                    .insert("Parent Relationship".to_owned(), json!(relationship));
            }
        }
        self.plan
    }
}

fn label_was_used(props: &Map<String, Value>) -> bool {
    props.contains_key("Subplan Name")
}

/// `InitPlan 1 (returns $0)`, `InitPlan 2`, `SubPlan 1`, `CTE totals`.
fn subplan_label(content: &str) -> Option<(String, &'static str)> {
    let numbered = |rest: &str| {
        rest.split(' ')
            .next()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    if content.strip_prefix("InitPlan ").is_some_and(numbered) {
        Some((content.to_owned(), "InitPlan"))
    } else if content.strip_prefix("SubPlan ").is_some_and(numbered) {
        Some((content.to_owned(), "SubPlan"))
    } else if content.starts_with("CTE ") && !content.contains(": ") {
        Some((content.to_owned(), "InitPlan"))
    } else {
        None
    }
}

/// `Worker 0:  actual time=...` → (0, `actual time=...`).
fn worker_line(content: &str) -> Option<(u64, &str)> {
    let rest = content.strip_prefix("Worker ")?;
    let (worker, rest) = rest.split_once(':')?;
    let worker = worker.parse().ok()?;
    Some((worker, rest.trim()))
}

fn split_label(content: &str) -> Option<(&str, &str)> {
    match content.split_once(": ") {
        Some((label, value)) => Some((label, value.trim())),
        None => content.strip_suffix(':').map(|label| (label, "")),
    }
}

/// The outcome of reading a property line.
enum Parsed {
    Known,
    /// A `Label: value` line with an unfamiliar label, kept with a guessed type.
    Unknown,
    /// A line that could not be read.
    Invalid,
}

/// Properties of a node or of one of its workers.
fn read_property(content: &str, props: &mut Map<String, Value>) -> Parsed {
    if let Some(actual) = content.strip_prefix("actual ") {
        // A worker's first line.
        return known(actual_values(actual, props));
    }
    let Some((label, value)) = split_label(content) else {
        return Parsed::Invalid;
    };
    let parsed = match label {
        "Output"
        | "Sort Key"
        | "Presorted Key"
        | "Group Key"
        | "Hash Key"
        | "Params Evaluated"
        | "Conflict Arbiter Indexes"
        | "Partition Key" => {
            props.insert(label.to_owned(), list(value));
            true
        }
        "Filter"
        | "Index Cond"
        | "Recheck Cond"
        | "Join Filter"
        | "Hash Cond"
        | "Merge Cond"
        | "One-Time Filter"
        | "Order By"
        | "TID Cond"
        | "Run Condition"
        | "Cache Key"
        | "Cache Mode"
        | "Function Call"
        | "Table Function Call"
        | "Window"
        | "Remote SQL"
        | "Relations"
        | "Conflict Resolution"
        | "Conflict Filter"
        | "Sampling" => {
            props.insert(label.to_owned(), json!(value));
            true
        }
        "Rows Removed by Filter"
        | "Rows Removed by Join Filter"
        | "Rows Removed by Index Recheck"
        | "Rows Removed by Conflict Filter"
        | "Heap Fetches"
        | "Workers Planned"
        | "Workers Launched"
        | "Subplans Removed"
        | "Index Searches"
        | "Tuples Inserted"
        | "Conflicting Tuples" => insert_number(props, label, value),
        "Inner Unique" | "Single Copy" | "Disabled" => match value {
            "true" | "false" => {
                props.insert(label.to_owned(), json!(value == "true"));
                true
            }
            _ => false,
        },
        "Sort Method" => sort_method(value, props),
        "Buckets" => hash(content, props),
        "Batches" | "Planned Partitions" => hash_aggregate(content, props),
        "Hits" => memoize(content, props),
        "Storage" => storage(content, props),
        "Heap Blocks" => heap_blocks(value, props),
        "Buffers" => buffers(value, props),
        "I/O Timings" => io_timings(value, props),
        "WAL" => wal(value, props),
        "Full-sort Groups" | "Pre-sorted Groups" => sort_groups(label, value, props),
        _ => {
            props.insert(label.to_owned(), guess(value));
            return Parsed::Unknown;
        }
    };
    known(parsed)
}

/// Lines under `Planning:` and `Serialization:`.
fn read_section_property(content: &str, props: &mut Map<String, Value>) -> Parsed {
    let Some((label, value)) = split_label(content) else {
        return Parsed::Invalid;
    };
    known(match label {
        "Buffers" => buffers(value, props),
        "I/O Timings" => io_timings(value, props),
        "Memory" => key_values(value, props, |key| match key {
            "used" => Some("Memory Used"),
            "allocated" => Some("Memory Allocated"),
            _ => None,
        }),
        _ => {
            props.insert(label.to_owned(), guess(value));
            return Parsed::Unknown;
        }
    })
}

/// Lines under `JIT:`.
fn read_jit_property(content: &str, props: &mut Map<String, Value>) -> Parsed {
    let Some((label, value)) = split_label(content) else {
        return Parsed::Invalid;
    };
    known(match label {
        "Functions" => insert_number(props, label, value),
        "Options" => {
            let mut options = Map::new();
            let ok = value
                .split(", ")
                .all(|option| match option.rsplit_once(' ') {
                    Some((name, flag @ ("true" | "false"))) => {
                        options.insert(name.to_owned(), json!(flag == "true"));
                        true
                    }
                    _ => false,
                });
            props.insert(label.to_owned(), Value::Object(options));
            ok
        }
        "Timing" => {
            let mut timing = Map::new();
            let ok = value.split(", ").all(|phase| {
                let Some((name, time)) = phase.split_once(' ') else {
                    return false;
                };
                let (time, deform) = match time.split_once(" (Deform ") {
                    Some((time, deform)) => (time, deform.strip_suffix(')').and_then(milliseconds)),
                    None => (time, None),
                };
                let Some(time) = milliseconds(time) else {
                    return false;
                };
                let value = match deform {
                    Some(deform) => json!({ "Deform": number(deform), "Total": number(time) }),
                    None => number(time),
                };
                timing.insert(name.to_owned(), value);
                true
            });
            props.insert(label.to_owned(), Value::Object(timing));
            ok
        }
        _ => {
            props.insert(label.to_owned(), guess(value));
            return Parsed::Unknown;
        }
    })
}

fn known(parsed: bool) -> Parsed {
    if parsed {
        Parsed::Known
    } else {
        Parsed::Invalid
    }
}

/// Reads a node line such as
/// `Index Scan using t_pkey on public.t  (cost=0.29..8.30 rows=1 width=4) (actual time=0.010..0.011 rows=1 loops=1)`.
/// Returns whether it looks like a plan node.
fn header_line(header: &str, props: &mut Map<String, Value>) -> bool {
    let mut rest = header.trim_end();
    let mut measured = false;
    if let Some(stripped) = rest.strip_suffix("(never executed)") {
        props.insert("Actual Rows".to_owned(), json!(0));
        props.insert("Actual Loops".to_owned(), json!(0));
        rest = stripped.trim_end();
        measured = true;
    } else if let Some(open) = rest.strip_suffix(')').and_then(|r| r.rfind("(actual ")) {
        if actual_values(&rest[open + "(actual ".len()..rest.len() - 1], props) {
            rest = rest[..open].trim_end();
            measured = true;
        }
    }
    let mut costed = false;
    if let Some(open) = rest.strip_suffix(')').and_then(|r| r.rfind("(cost=")) {
        if cost_values(&rest[open + "(cost=".len()..rest.len() - 1], props) {
            rest = rest[..open].trim_end();
            costed = true;
        }
    }
    let known = node_name(rest, props);
    known || measured || costed
}

/// `time=0.010..0.011 rows=1 loops=1` or, with `TIMING OFF`, `rows=1 loops=1`.
fn actual_values(text: &str, props: &mut Map<String, Value>) -> bool {
    let mut values = Map::new();
    for token in text.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            return false;
        };
        match key {
            "time" => {
                let Some((startup, total)) = value.split_once("..") else {
                    return false;
                };
                let (Some(startup), Some(total)) = (float(startup), float(total)) else {
                    return false;
                };
                values.insert("Actual Startup Time".to_owned(), number(startup));
                values.insert("Actual Total Time".to_owned(), number(total));
            }
            "rows" => match float(value) {
                Some(rows) => {
                    values.insert("Actual Rows".to_owned(), number(rows));
                }
                None => return false,
            },
            "loops" => match float(value) {
                Some(loops) => {
                    values.insert("Actual Loops".to_owned(), number(loops));
                }
                None => return false,
            },
            _ => return false,
        }
    }
    if !values.contains_key("Actual Rows") || !values.contains_key("Actual Loops") {
        return false;
    }
    props.extend(values);
    true
}

/// `0.29..8.30 rows=1 width=4`.
fn cost_values(text: &str, props: &mut Map<String, Value>) -> bool {
    let mut tokens = text.split_whitespace();
    let costs = tokens.next().and_then(|costs| costs.split_once(".."));
    let (Some((startup, total)), Some(rows), Some(width), None) =
        (costs, tokens.next(), tokens.next(), tokens.next())
    else {
        return false;
    };
    let values = (
        float(startup),
        float(total),
        rows.strip_prefix("rows=").and_then(float),
        width.strip_prefix("width=").and_then(float),
    );
    let (Some(startup), Some(total), Some(rows), Some(width)) = values else {
        return false;
    };
    props.insert("Startup Cost".to_owned(), number(startup));
    props.insert("Total Cost".to_owned(), number(total));
    props.insert("Plan Rows".to_owned(), number(rows));
    props.insert("Plan Width".to_owned(), number(width));
    true
}

/// Node types without a target.
const SIMPLE_NODES: [&str; 19] = [
    "Result",
    "ProjectSet",
    "Append",
    "Merge Append",
    "Recursive Union",
    "BitmapAnd",
    "BitmapOr",
    "Sort",
    "Incremental Sort",
    "Group",
    "Unique",
    "Hash",
    "Materialize",
    "Memoize",
    "Limit",
    "LockRows",
    "Gather",
    "Gather Merge",
    "WindowAgg",
];

/// Scans printed as `<type> on <target>`.
const SCANS: [&str; 13] = [
    "Seq Scan",
    "Sample Scan",
    "Bitmap Heap Scan",
    "Tid Scan",
    "Tid Range Scan",
    "Subquery Scan",
    "Function Scan",
    "Table Function Scan",
    "Values Scan",
    "CTE Scan",
    "Named Tuplestore Scan",
    "WorkTable Scan",
    "Foreign Scan",
];

/// Splits a node name such as `Parallel Hash Right Anti Join` or
/// `Index Scan Backward using t_pkey on public.t t1` into JSON properties.
/// Returns whether the node type is known.
fn node_name(name: &str, props: &mut Map<String, Value>) -> bool {
    let set = |props: &mut Map<String, Value>, key: &str, value: &str| {
        props.insert(key.to_owned(), json!(value));
    };
    let mut name = name;
    if let Some(rest) = name.strip_prefix("Parallel ") {
        props.insert("Parallel Aware".to_owned(), json!(true));
        name = rest;
    }
    if let Some(rest) = name.strip_prefix("Async ") {
        props.insert("Async Capable".to_owned(), json!(true));
        name = rest;
    }

    if let Some((node_type, join_type)) = join(name) {
        set(props, "Node Type", node_type);
        set(props, "Join Type", join_type);
        return true;
    }
    if let Some((partial_mode, strategy)) = aggregate(name) {
        set(props, "Node Type", "Aggregate");
        set(props, "Strategy", strategy);
        set(props, "Partial Mode", partial_mode);
        return true;
    }
    if let Some((strategy, command)) = set_operation(name) {
        set(props, "Node Type", "SetOp");
        set(props, "Strategy", strategy);
        set(props, "Command", command);
        return true;
    }
    for node_type in ["Index Only Scan", "Index Scan"] {
        let Some(rest) = name.strip_prefix(node_type) else {
            continue;
        };
        let (direction, rest) = match rest.strip_prefix(" Backward") {
            Some(rest) => ("Backward", rest),
            None => ("Forward", rest),
        };
        let Some((index, rest)) = rest.strip_prefix(" using ").and_then(identifier) else {
            continue;
        };
        set(props, "Node Type", node_type);
        set(props, "Scan Direction", direction);
        set(props, "Index Name", &index);
        if let Some(target_text) = rest.strip_prefix(" on ") {
            target(node_type, target_text, props);
        }
        return true;
    }
    if let Some((index, _)) = name
        .strip_prefix("Bitmap Index Scan on ")
        .and_then(identifier)
    {
        set(props, "Node Type", "Bitmap Index Scan");
        set(props, "Index Name", &index);
        return true;
    }
    for (prefix, node_type, operation) in [
        ("Insert on ", "ModifyTable", "Insert"),
        ("Update on ", "ModifyTable", "Update"),
        ("Delete on ", "ModifyTable", "Delete"),
        ("Merge on ", "ModifyTable", "Merge"),
        ("Foreign Insert on ", "Foreign Scan", "Insert"),
        ("Foreign Update on ", "Foreign Scan", "Update"),
        ("Foreign Delete on ", "Foreign Scan", "Delete"),
    ] {
        if let Some(target_text) = name.strip_prefix(prefix) {
            set(props, "Node Type", node_type);
            set(props, "Operation", operation);
            target(node_type, target_text, props);
            return true;
        }
    }
    if let Some(rest) = name.strip_prefix("Custom Scan (") {
        if let Some((provider, rest)) = rest.split_once(')') {
            set(props, "Node Type", "Custom Scan");
            set(props, "Custom Plan Provider", provider);
            if let Some(target_text) = rest.strip_prefix(" on ") {
                target("Custom Scan", target_text, props);
            }
            return true;
        }
    }
    for scan in SCANS {
        if name == scan {
            set(props, "Node Type", scan);
            return true;
        }
        if let Some(target_text) = name
            .strip_prefix(scan)
            .and_then(|rest| rest.strip_prefix(" on "))
        {
            set(props, "Node Type", scan);
            target(scan, target_text, props);
            return true;
        }
    }
    if SIMPLE_NODES.contains(&name) {
        set(props, "Node Type", name);
        return true;
    }
    set(props, "Node Type", name);
    false
}

/// `Nested Loop`, `Nested Loop Left Join`, `Hash Join`, `Merge Anti Join`, ...
fn join(name: &str) -> Option<(&'static str, &str)> {
    for (prefix, node_type) in [
        ("Nested Loop", "Nested Loop"),
        ("Hash", "Hash Join"),
        ("Merge", "Merge Join"),
    ] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            if prefix == "Nested Loop" {
                return Some((node_type, "Inner"));
            }
            continue;
        }
        let Some(join_type) = rest
            .strip_prefix(' ')
            .and_then(|rest| rest.strip_suffix("Join"))
            .map(str::trim)
        else {
            continue;
        };
        let join_type = if join_type.is_empty() {
            "Inner"
        } else {
            join_type
        };
        if matches!(
            join_type,
            "Inner" | "Left" | "Full" | "Right" | "Semi" | "Anti" | "Right Semi" | "Right Anti"
        ) {
            return Some((node_type, join_type));
        }
    }
    None
}

/// `Aggregate`, `Partial HashAggregate`, `Finalize GroupAggregate`, ...
/// → (partial mode, strategy).
fn aggregate(name: &str) -> Option<(&'static str, &'static str)> {
    let (partial_mode, rest) = if let Some(rest) = name.strip_prefix("Partial ") {
        ("Partial", rest)
    } else if let Some(rest) = name.strip_prefix("Finalize ") {
        ("Finalize", rest)
    } else {
        ("Simple", name)
    };
    let strategy = match rest {
        "Aggregate" => "Plain",
        "GroupAggregate" => "Sorted",
        "HashAggregate" => "Hashed",
        "MixedAggregate" => "Mixed",
        _ => return None,
    };
    Some((partial_mode, strategy))
}

/// `HashSetOp Except`, `SetOp Intersect All` → (strategy, command).
fn set_operation(name: &str) -> Option<(&'static str, &str)> {
    let (strategy, command) = if let Some(command) = name.strip_prefix("HashSetOp ") {
        ("Hashed", command)
    } else {
        ("Sorted", name.strip_prefix("SetOp ")?)
    };
    matches!(
        command,
        "Intersect" | "Intersect All" | "Except" | "Except All"
    )
    .then_some((strategy, command))
}

/// `[schema.]object [alias]` after ` on `. Text plans print the alias only
/// when it differs from the object name; JSON plans always name it.
fn target(node_type: &str, text: &str, props: &mut Map<String, Value>) {
    let tag = match node_type {
        "Function Scan" => Some("Function Name"),
        "Table Function Scan" => Some("Table Function Name"),
        "CTE Scan" | "WorkTable Scan" => Some("CTE Name"),
        "Named Tuplestore Scan" => Some("Tuplestore Name"),
        "Subquery Scan" | "Values Scan" => None,
        _ => Some("Relation Name"),
    };
    let Some((first, rest)) = identifier(text) else {
        return;
    };
    let Some(tag) = tag else {
        props.insert("Alias".to_owned(), json!(first));
        return;
    };
    let (schema, object, rest) = match rest.strip_prefix('.').and_then(identifier) {
        Some((object, rest)) => (Some(first), object, rest),
        None => (None, first, rest),
    };
    let alias = rest
        .strip_prefix(' ')
        .and_then(identifier)
        .map_or_else(|| object.clone(), |(alias, _)| alias);
    props.insert(tag.to_owned(), json!(object));
    if let Some(schema) = schema {
        props.insert("Schema".to_owned(), json!(schema));
    }
    props.insert("Alias".to_owned(), json!(alias));
}

/// Reads one identifier, quoted (`"Order"`, `"a ""b"""`) or not, and
/// returns it with the rest of the text.
fn identifier(text: &str) -> Option<(String, &str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.char_indices();
        while let Some((position, c)) = chars.next() {
            if c == '"' {
                if quoted[position + 1..].starts_with('"') {
                    name.push('"');
                    chars.next();
                } else {
                    return Some((name, &quoted[position + 1..]));
                }
            } else {
                name.push(c);
            }
        }
        None
    } else {
        let end = text.find([' ', '.']).unwrap_or(text.len());
        (end > 0).then(|| (text[..end].to_owned(), &text[end..]))
    }
}

/// Splits a list such as an `Output` at top-level commas.
fn list(value: &str) -> Value {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut chars = value.char_indices().peekable();
    while let Some((position, c)) = chars.next() {
        match quote {
            Some(q) if c == q => {
                if chars.peek().is_some_and(|&(_, next)| next == q) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 && value[position + 1..].starts_with(' ') => {
                    items.push(value[start..position].trim());
                    start = position + 2;
                }
                _ => {}
            },
        }
    }
    items.push(value[start..].trim());
    json!(items)
}

/// `quicksort  Memory: 25kB`.
fn sort_method(value: &str, props: &mut Map<String, Value>) -> bool {
    let Some((method, space)) = value.split_once("  ") else {
        props.insert("Sort Method".to_owned(), json!(value));
        return true;
    };
    let Some((space_type, used)) = space.split_once(": ") else {
        return false;
    };
    let Some(used) = kilobytes(used) else {
        return false;
    };
    props.insert("Sort Method".to_owned(), json!(method));
    props.insert("Sort Space Type".to_owned(), json!(space_type));
    props.insert("Sort Space Used".to_owned(), number(used));
    true
}

/// `Label: value` segments separated by two spaces.
fn segments(content: &str) -> Option<Vec<(&str, &str)>> {
    content
        .split("  ")
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.split_once(": "))
        .collect()
}

/// `Buckets: 4096 (originally 1024)  Batches: 4 (originally 1)  Memory Usage: 89kB`.
fn hash(content: &str, props: &mut Map<String, Value>) -> bool {
    let Some(segments) = segments(content) else {
        return false;
    };
    segments.into_iter().all(|(label, value)| {
        let (current, original) = match value.split_once(" (originally ") {
            Some((current, original)) => (current, original.strip_suffix(')').unwrap_or(original)),
            None => (value, value),
        };
        match label {
            "Buckets" | "Batches" => {
                let (Some(current), Some(original)) = (float(current), float(original)) else {
                    return false;
                };
                props.insert(format!("Hash {label}"), number(current));
                props.insert(format!("Original Hash {label}"), number(original));
                true
            }
            "Memory Usage" => insert_kilobytes(props, "Peak Memory Usage", value),
            _ => false,
        }
    })
}

/// `Planned Partitions: 4  Batches: 5  Memory Usage: 4401kB  Disk Usage: 3456kB`.
fn hash_aggregate(content: &str, props: &mut Map<String, Value>) -> bool {
    let Some(segments) = segments(content) else {
        return false;
    };
    segments.into_iter().all(|(label, value)| match label {
        "Planned Partitions" => insert_number(props, label, value),
        "Batches" => insert_number(props, "HashAgg Batches", value),
        "Memory Usage" => insert_kilobytes(props, "Peak Memory Usage", value),
        "Disk Usage" => insert_kilobytes(props, "Disk Usage", value),
        _ => false,
    })
}

/// `Hits: 15000  Misses: 5000  Evictions: 0  Overflows: 0  Memory Usage: 583kB`.
fn memoize(content: &str, props: &mut Map<String, Value>) -> bool {
    let Some(segments) = segments(content) else {
        return false;
    };
    segments.into_iter().all(|(label, value)| match label {
        "Hits" | "Misses" | "Evictions" | "Overflows" => {
            insert_number(props, &format!("Cache {label}"), value)
        }
        "Memory Usage" => insert_kilobytes(props, "Peak Memory Usage", value),
        _ => false,
    })
}

/// `Storage: Memory  Maximum Storage: 17kB`.
fn storage(content: &str, props: &mut Map<String, Value>) -> bool {
    let Some(segments) = segments(content) else {
        return false;
    };
    segments.into_iter().all(|(label, value)| match label {
        "Storage" => {
            props.insert(label.to_owned(), json!(value));
            true
        }
        "Maximum Storage" => insert_kilobytes(props, label, value),
        _ => false,
    })
}

/// `exact=875 lossy=123`.
fn heap_blocks(value: &str, props: &mut Map<String, Value>) -> bool {
    key_values(value, props, |key| match key {
        "exact" => Some("Exact Heap Blocks"),
        "lossy" => Some("Lossy Heap Blocks"),
        _ => None,
    })
}

/// `shared hit=125 read=2353 dirtied=1 written=2, local hit=1, temp read=3 written=4`.
fn buffers(value: &str, props: &mut Map<String, Value>) -> bool {
    value.split(", ").all(|group| {
        let Some((scope, counts)) = group.split_once(' ') else {
            return false;
        };
        let scope = match scope {
            "shared" => "Shared",
            "local" => "Local",
            "temp" => "Temp",
            _ => return false,
        };
        key_values(counts, props, |key| {
            let kind = match key {
                "hit" => "Hit",
                "read" => "Read",
                "dirtied" => "Dirtied",
                "written" => "Written",
                _ => return None,
            };
            Some(match (scope, kind) {
                ("Shared", "Hit") => "Shared Hit Blocks",
                ("Shared", "Read") => "Shared Read Blocks",
                ("Shared", "Dirtied") => "Shared Dirtied Blocks",
                ("Shared", "Written") => "Shared Written Blocks",
                ("Local", "Hit") => "Local Hit Blocks",
                ("Local", "Read") => "Local Read Blocks",
                ("Local", "Dirtied") => "Local Dirtied Blocks",
                ("Local", "Written") => "Local Written Blocks",
                ("Temp", "Read") => "Temp Read Blocks",
                ("Temp", "Written") => "Temp Written Blocks",
                _ => return None,
            })
        })
    })
}

/// `read=3.262 write=0.1` (up to PostgreSQL 14) or
/// `shared read=2.671 write=0.165, local read=1.0, temp read=0.2 write=0.3`.
/// Shared and combined shared/local timings are both read as shared.
fn io_timings(value: &str, props: &mut Map<String, Value>) -> bool {
    value.split(", ").all(|group| {
        let (scope, times) = match group.split_once(' ') {
            Some((scope, times)) if !scope.contains('=') => (scope, times),
            _ => ("shared", group),
        };
        let scope = match scope {
            "shared" | "shared/local" => "Shared",
            "local" => "Local",
            "temp" => "Temp",
            _ => return false,
        };
        key_values(times, props, |key| {
            Some(match (scope, key) {
                ("Shared", "read") => "Shared I/O Read Time",
                ("Shared", "write") => "Shared I/O Write Time",
                ("Local", "read") => "Local I/O Read Time",
                ("Local", "write") => "Local I/O Write Time",
                ("Temp", "read") => "Temp I/O Read Time",
                ("Temp", "write") => "Temp I/O Write Time",
                _ => return None,
            })
        })
    })
}

/// `records=5000 fpi=3 bytes=395000 buffers full=2`.
fn wal(value: &str, props: &mut Map<String, Value>) -> bool {
    key_values(
        &value.replace("buffers full=", "buffers_full="),
        props,
        |key| match key {
            "records" => Some("WAL Records"),
            "fpi" => Some("WAL FPI"),
            "bytes" => Some("WAL Bytes"),
            "buffers_full" => Some("WAL Buffers Full"),
            _ => None,
        },
    )
}

/// `4  Sort Method: quicksort  Average Memory: 29kB  Peak Memory: 29kB`
/// after `Full-sort Groups:` or `Pre-sorted Groups:`.
fn sort_groups(label: &str, value: &str, props: &mut Map<String, Value>) -> bool {
    let mut parts = value.split("  ").filter(|part| !part.is_empty());
    let Some(count) = parts.next().and_then(float) else {
        return false;
    };
    let mut group = Map::new();
    group.insert("Group Count".to_owned(), number(count));
    for part in parts {
        let Some((key, text)) = part.split_once(": ") else {
            return false;
        };
        let (space, measure) = match key {
            "Sort Method" | "Sort Methods" => {
                let methods: Vec<&str> = text.split(", ").collect();
                group.insert("Sort Methods Used".to_owned(), json!(methods));
                continue;
            }
            "Average Memory" => ("Sort Space Memory", "Average Sort Space Used"),
            "Peak Memory" => ("Sort Space Memory", "Peak Sort Space Used"),
            "Average Disk" => ("Sort Space Disk", "Average Sort Space Used"),
            "Peak Disk" => ("Sort Space Disk", "Peak Sort Space Used"),
            _ => return false,
        };
        let Some(amount) = kilobytes(text) else {
            return false;
        };
        let space = group
            .entry(space)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(space) = space {
            space.insert(measure.to_owned(), number(amount));
        }
    }
    props.insert(label.to_owned(), Value::Object(group));
    true
}

/// `a = '1', b = 'x, y'`.
fn settings(value: &str) -> Option<Map<String, Value>> {
    let mut settings = Map::new();
    let mut rest = value;
    while !rest.is_empty() {
        let (name, after) = rest.split_once(" = '")?;
        let mut setting = String::new();
        let mut chars = after.char_indices();
        let mut end = None;
        while let Some((position, c)) = chars.next() {
            if c == '\'' {
                if after[position + 1..].starts_with('\'') {
                    setting.push('\'');
                    chars.next();
                } else {
                    end = Some(position + 1);
                    break;
                }
            } else {
                setting.push(c);
            }
        }
        settings.insert(name.trim().to_owned(), json!(setting));
        rest = after[end?..].strip_prefix(", ").unwrap_or(&after[end?..]);
    }
    Some(settings)
}

/// `Trigger RI_ConstraintTrigger_a_16417 for constraint fk: time=301.058 calls=20`.
fn trigger(content: &str) -> Option<Map<String, Value>> {
    let rest = content.strip_prefix("Trigger")?;
    let (description, stats) = rest.rsplit_once(": ")?;
    let mut trigger = Map::new();
    let (description, relation) = match description.rsplit_once(" on ") {
        Some((description, relation)) => (description, Some(relation)),
        None => (description, None),
    };
    let (name, constraint) = match description.split_once("for constraint ") {
        Some((name, constraint)) => (name.trim(), Some(constraint.trim())),
        None => (description.trim(), None),
    };
    if !name.is_empty() {
        trigger.insert("Trigger Name".to_owned(), json!(name));
    }
    if let Some(constraint) = constraint {
        trigger.insert("Constraint Name".to_owned(), json!(constraint));
    }
    if let Some(relation) = relation {
        trigger.insert("Relation".to_owned(), json!(relation.trim()));
    }
    key_values(stats, &mut trigger, |key| match key {
        "time" => Some("Time"),
        "calls" => Some("Calls"),
        _ => None,
    })
    .then_some(trigger)
}

/// `time=0.621 ms  output=165kB  format=text`.
fn serialization(value: &str) -> Option<Map<String, Value>> {
    let mut serialization = Map::new();
    for part in value.split("  ").filter(|part| !part.is_empty()) {
        let (key, text) = part.split_once('=')?;
        match key {
            "time" => {
                serialization.insert("Time".to_owned(), number(milliseconds(text)?));
            }
            "output" => {
                serialization.insert("Output Volume".to_owned(), number(kilobytes(text)?));
            }
            "format" => {
                serialization.insert("Format".to_owned(), json!(text));
            }
            _ => return None,
        }
    }
    Some(serialization)
}

/// Reads `key=number` tokens into the properties named by `name`.
fn key_values(
    text: &str,
    props: &mut Map<String, Value>,
    name: impl Fn(&str) -> Option<&'static str>,
) -> bool {
    text.split_whitespace().all(|token| {
        let Some((key, value)) = token.split_once('=') else {
            return false;
        };
        let (Some(name), Some(value)) = (name(key), kilobytes(value)) else {
            return false;
        };
        props.insert(name.to_owned(), number(value));
        true
    })
}

fn insert_number(props: &mut Map<String, Value>, key: &str, value: &str) -> bool {
    float(value)
        .map(|value| props.insert(key.to_owned(), number(value)))
        .is_some()
}

fn insert_kilobytes(props: &mut Map<String, Value>, key: &str, value: &str) -> bool {
    kilobytes(value)
        .map(|value| props.insert(key.to_owned(), number(value)))
        .is_some()
}

fn number_value(value: f64) -> Value {
    number(value)
}

fn float(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// `25kB` or `25` → 25.
fn kilobytes(text: &str) -> Option<f64> {
    float(text.strip_suffix("kB").unwrap_or(text))
}

/// `0.520 ms` → 0.52.
fn milliseconds(text: &str) -> Option<f64> {
    float(text.trim().strip_suffix("ms").unwrap_or(text).trim())
}

/// A value of an unfamiliar property: a number or a boolean when it looks
/// like one, otherwise text.
fn guess(value: &str) -> Value {
    match value {
        "true" => json!(true),
        "false" => json!(false),
        _ => float(value).map_or_else(|| json!(value), number),
    }
}
