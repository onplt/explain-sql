//! Requests, from server logs: the statements an application ran for one
//! request, and the loops among them. A loop is the same statement run
//! again and again in one request with another value each time, as an ORM
//! runs it when it loads related rows one parent at a time (N+1). Each
//! statement of a loop is fast, so a plan, or a list of statements by their
//! mean time, says it is fine: the cost shows only per request.
//!
//! Statements go together in a request by the trace id of their
//! sqlcommenter `traceparent` tag; otherwise by the transaction they ran in
//! (`%v`, or the jsonlog and csvlog field), in their session; otherwise by
//! their session (`%c`, or the process), split where it was idle for longer
//! than a gap.
//!
//! For a loop, this module writes the batched statement, which runs once
//! for all the values: `= ANY($1)` in place of `= $1`, or for a statement
//! whose rows must stay per value (a `LIMIT`, an aggregate), the statement
//! in a `LATERAL` subquery over `unnest($1)`. Connected mode measures both;
//! [`proof`] sums up what it measured.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::compare;
use crate::ir::Plan;
use crate::params;
use crate::pg::LoggedStatement;
use crate::timeline;

/// How statements are grouped and loops found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// The fewest runs of a statement in one request that make a loop.
    pub min_runs: usize,
    /// Statements of a session that ran outside a transaction and without
    /// a trace go in one request while it was idle for no longer than this
    /// between them, in milliseconds.
    pub gap: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            min_runs: 3,
            gap: 50.0,
        }
    }
}

/// The requests of a log and the loops in them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    /// How many statements were read.
    pub statements: usize,
    /// The first and last timestamps, as the log prints them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// In time order.
    pub requests: Vec<Request>,
    /// The loops, those with other values each run first, then those that
    /// repeat the same values; the most runs in all first.
    pub loops: Vec<Loop>,
}

/// The statements of one request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    /// What put its statements together.
    pub by: Grouping,
    /// The trace id of its sqlcommenter `traceparent` tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// Where in the application it comes from, from sqlcommenter tags: the
    /// route, or the controller and action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The session of its first statement: its id, or its process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// When its first statement ended, as the log prints it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Its statements, in time order: indexes into the statements read.
    pub statements: Vec<usize>,
    /// Their logged durations summed, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Its statements by shape, in the order they first ran.
    pub shapes: Vec<Shape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    /// The trace id of their `traceparent` tag.
    Trace,
    /// The transaction they ran in.
    Transaction,
    /// Their session, while it was not idle for longer than the gap.
    Session,
}

/// The runs of one statement in a request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// Without comments, literal values or parameters.
    pub text: String,
    pub runs: usize,
    /// Their logged durations summed, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// The loop these runs are part of: an index into the loops.
    #[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
    pub in_loop: Option<usize>,
    /// What tells its statements from others'.
    #[serde(skip)]
    key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    /// Another value on most runs: one statement per row (N+1).
    Loop,
    /// The same values on every run: the same rows read again.
    Repeat,
}

/// A statement that ran in a loop, in one request or many.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Loop {
    pub kind: LoopKind,
    /// Without comments, literal values or parameters.
    pub text: String,
    /// What it does: `select`, `insert`, `update`, `delete`.
    pub command: String,
    /// The requests it looped in, and its runs in them.
    pub requests: usize,
    pub runs: usize,
    /// The fewest and the most runs in one of those requests.
    pub fewest: usize,
    pub most: usize,
    /// The logged durations of those runs, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// The request with the most runs of it: an index into the requests.
    pub example: usize,
    /// The statement that ran before the loop in that request, if it was
    /// another: often the one that read the parents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The statement of one run, as it is prepared: with `$1` for each
    /// value that varies when the log has the values written in.
    pub single: String,
    /// The parameters of `single` whose values vary from run to run.
    pub varying: Vec<usize>,
    /// The values of each run of the example request, by parameter of
    /// `single`. Left out of reports: they are the application's data.
    #[serde(skip)]
    pub values: Vec<Vec<Option<String>>>,
    /// The column a varying value is compared with in `single`, as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// sqlcommenter tags of its first run, the trace context left out.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// The statement that does the work of all the runs at once, or why
    /// there is none (`{"not_batched": "…"}` in JSON).
    #[serde(serialize_with = "batched_json")]
    pub batched: Result<Batched, String>,
    /// How to make the application run it.
    pub advice: Vec<String>,
    /// The foreign key between the loop's table and its parents', from the
    /// catalog in connected mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    /// The batched statement against the runs, measured in connected mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn batched_json<S: serde::Serializer>(
    batched: &Result<Batched, String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct NotBatched<'a> {
        not_batched: &'a str,
    }
    match batched {
        Ok(batched) => batched.serialize(serializer),
        Err(reason) => NotBatched {
            not_batched: reason,
        }
        .serialize(serializer),
    }
}

/// A statement that does the work of a loop's runs at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Batched {
    pub form: Form,
    /// The statement, each varying parameter taking an array of the values:
    /// with its type when it is known (`$1::integer[]`).
    pub sql: String,
    /// What the application must do with its rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    /// `col = ANY($1)` in place of `col = $1`: what an ORM's batch fetching
    /// sends.
    Any,
    /// The statement in a `LATERAL` subquery, once per value of
    /// `unnest($1)`: each value keeps its own `LIMIT` or aggregate.
    Lateral,
}

/// A foreign key between the table a loop reads and another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reference {
    pub constraint: String,
    /// The referencing table and column.
    pub from_table: String,
    pub from_column: String,
    /// The referenced table and column.
    pub to_table: String,
    pub to_column: String,
    /// Whether the loop reads the referencing side, the children of each
    /// parent (a collection); else the referenced side, the parent of each
    /// child.
    pub children: bool,
}

/// The batched statement against the runs it replaces, measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proof {
    /// As it ran.
    pub sql: String,
    /// The runs in the example request, and how many of them were measured;
    /// when fewer, the figures of the runs are scaled to all of them.
    pub runs: usize,
    pub measured: usize,
    /// The distinct values the batched statement ran with.
    pub values: usize,
    /// All the runs.
    pub single: Side,
    pub batched: Side,
    /// The median time of a round trip to the server from this machine
    /// (`SELECT 1`), in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_trip: Option<f64>,
    /// The time the batched statement saves, round trips included, in
    /// milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<f64>,
    /// The batched statement reads its tables another way than one run.
    pub plan_changed: bool,
}

/// What one side of a proof cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Side {
    /// Planning and execution, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// Pages read, from cache or disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    /// How it reads its tables.
    pub access: String,
}

/// The requests of the statements and the loops in them.
pub fn profile(statements: &[LoggedStatement], options: Options) -> Profile {
    let mut order: Vec<usize> = (0..statements.len()).collect();
    order.sort_by_key(|&index| (instant(&statements[index]), index));
    let stamps: Vec<&str> = order
        .iter()
        .filter_map(|&index| statements[index].meta.timestamp.as_deref())
        .collect();

    let mut requests: Vec<Request> = group(statements, &order, options)
        .into_iter()
        .map(|(by, indexes)| request(statements, by, indexes))
        .collect();
    let loops = loops(statements, &mut requests, options);
    Profile {
        statements: statements.len(),
        from: stamps.first().map(|stamp| (*stamp).to_owned()),
        to: stamps.last().map(|stamp| (*stamp).to_owned()),
        requests,
        loops,
    }
}

/// When a statement ended, as a number that orders.
fn instant(statement: &LoggedStatement) -> Option<i64> {
    timeline::instant(statement.meta.timestamp.as_deref())
}

/// When a statement started: when it ended, less its duration.
fn started(statement: &LoggedStatement) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    let end = instant(statement)? as f64;
    Some(end - statement.meta.duration().unwrap_or(0.0))
}

/// The trace id of a statement's `traceparent` tag.
fn trace(statement: &LoggedStatement) -> Option<String> {
    timeline::tags(&statement.text)
        .get("traceparent")
        .and_then(|parent| timeline::trace_id(parent))
        .map(str::to_lowercase)
}

/// What tells a statement's session from others: its session id, or else
/// its process.
fn session(statement: &LoggedStatement) -> Option<String> {
    match (&statement.meta.session, statement.meta.pid) {
        (Some(session), _) => Some(session.clone()),
        (None, Some(pid)) => Some(format!("pid {pid}")),
        (None, None) => None,
    }
}

/// The statements of each request, in time order, the requests in the
/// order they started.
fn group(
    statements: &[LoggedStatement],
    order: &[usize],
    options: Options,
) -> Vec<(Grouping, Vec<usize>)> {
    let mut groups: Vec<(Grouping, Vec<usize>)> = Vec::new();
    let mut traces: BTreeMap<String, usize> = BTreeMap::new();
    let mut untraced: Vec<usize> = Vec::new();
    for &index in order {
        match trace(&statements[index]) {
            Some(trace) => {
                let at = *traces.entry(trace).or_insert_with(|| {
                    groups.push((Grouping::Trace, Vec::new()));
                    groups.len() - 1
                });
                groups[at].1.push(index);
            }
            None => untraced.push(index),
        }
    }

    // A transaction of several statements is a request.
    let mut statements_in: BTreeMap<(Option<String>, String), usize> = BTreeMap::new();
    for &index in &untraced {
        if let Some(vxid) = &statements[index].meta.vxid {
            *statements_in
                .entry((session(&statements[index]), vxid.clone()))
                .or_default() += 1;
        }
    }
    // Per session, the request statements go on with.
    let mut open: BTreeMap<Option<String>, usize> = BTreeMap::new();
    for &index in &untraced {
        let statement = &statements[index];
        let session = session(statement);
        let transaction = statement.meta.vxid.as_ref().filter(|vxid| {
            statements_in
                .get(&(session.clone(), (*vxid).clone()))
                .is_some_and(|&count| count > 1)
        });
        let current = open.get(&session).copied();
        let joins = current.is_some_and(|at| {
            let (by, members) = &groups[at];
            let last = &statements[*members.last().expect("a request has statements")];
            match (by, transaction) {
                (Grouping::Transaction, Some(vxid)) => last.meta.vxid.as_ref() == Some(vxid),
                // The COMMIT of a simple query logs no transaction.
                (Grouping::Transaction, None) => ends_transaction(&statement.text),
                (Grouping::Session, None) => match (instant(last), started(statement)) {
                    #[allow(clippy::cast_precision_loss)]
                    (Some(end), Some(start)) => start - end as f64 <= options.gap,
                    _ => true,
                },
                _ => false,
            }
        });
        if joins {
            groups[current.expect("joins an open request")]
                .1
                .push(index);
        } else {
            let by = if transaction.is_some() {
                Grouping::Transaction
            } else {
                Grouping::Session
            };
            groups.push((by, vec![index]));
            open.insert(session, groups.len() - 1);
        }
    }

    let position: BTreeMap<usize, usize> = order
        .iter()
        .enumerate()
        .map(|(position, &index)| (index, position))
        .collect();
    groups.sort_by_key(|(_, members)| position[&members[0]]);
    groups
}

fn request(statements: &[LoggedStatement], by: Grouping, indexes: Vec<usize>) -> Request {
    let first = &statements[indexes[0]];
    let tags = indexes
        .iter()
        .map(|&index| timeline::tags(&statements[index].text))
        .find(|tags| !tags.is_empty())
        .unwrap_or_default();
    let label = ["route", "http.route", "http_route"]
        .iter()
        .find_map(|key| tags.get(*key).cloned())
        .or_else(|| match (tags.get("controller"), tags.get("action")) {
            (Some(controller), Some(action)) => Some(format!("{controller}#{action}")),
            (Some(controller), None) => Some(controller.clone()),
            (None, _) => None,
        });
    let mut shapes: Vec<Shape> = Vec::new();
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for &index in &indexes {
        let statement = &statements[index];
        let key = key(&statement.text);
        let at = match shapes.iter().position(|shape| shape.key == key) {
            Some(at) => at,
            None => {
                shapes.push(Shape {
                    text: timeline::normalize(&statement.text),
                    runs: 0,
                    total: None,
                    in_loop: None,
                    key,
                });
                runs.push(Vec::new());
                shapes.len() - 1
            }
        };
        shapes[at].runs += 1;
        runs[at].push(index);
    }
    for (shape, runs) in shapes.iter_mut().zip(&runs) {
        shape.total = total(statements, runs);
    }
    let total = total(statements, &indexes);
    Request {
        by,
        trace: (by == Grouping::Trace).then(|| trace(first)).flatten(),
        label,
        session: session(first),
        application: indexes
            .iter()
            .find_map(|&index| statements[index].meta.application.clone()),
        at: first.meta.timestamp.clone(),
        statements: indexes,
        total,
        shapes,
    }
}

/// The logged durations of statements summed, in milliseconds, when any
/// is known.
fn total(statements: &[LoggedStatement], indexes: &[usize]) -> Option<f64> {
    let known: Vec<u64> = indexes
        .iter()
        .filter_map(|&index| statements[index].meta.duration_us)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    (!known.is_empty()).then(|| known.iter().sum::<u64>() as f64 / 1000.0)
}

/// What tells statements of the same shape apart.
fn key(text: &str) -> String {
    timeline::normalize(text).to_lowercase()
}

/// Statements that are not the application's queries: transaction
/// control, settings, a connection check.
fn is_utility(text: &str) -> bool {
    let words = top_words(&tokens(text));
    let first = words.first().map(String::as_str).unwrap_or("");
    matches!(
        first,
        "begin"
            | "start"
            | "commit"
            | "end"
            | "rollback"
            | "abort"
            | "savepoint"
            | "release"
            | "set"
            | "reset"
            | "show"
            | "discard"
            | "deallocate"
            | "prepare"
            | "listen"
            | "unlisten"
    ) || (words.len() == 1 && first == "select")
}

fn ends_transaction(text: &str) -> bool {
    let words = top_words(&tokens(text));
    matches!(
        words.first().map(String::as_str),
        Some("commit" | "end" | "rollback" | "abort")
    )
}

/// The loops of the requests, the one with the most runs first; marks the
/// shapes of the requests that are part of one.
fn loops(statements: &[LoggedStatement], requests: &mut [Request], options: Options) -> Vec<Loop> {
    // Per shape, the requests it looped in and whether its values varied.
    struct Found {
        key: String,
        /// (request, shape in it, its runs, values varied)
        requests: Vec<(usize, usize, Vec<usize>, bool)>,
    }
    let mut found: Vec<Found> = Vec::new();
    for (number, request) in requests.iter().enumerate() {
        for (at, shape) in request.shapes.iter().enumerate() {
            if shape.runs < options.min_runs.max(2) {
                continue;
            }
            let runs: Vec<usize> = request
                .statements
                .iter()
                .copied()
                .filter(|&index| key(&statements[index].text) == shape.key)
                .collect();
            if is_utility(&statements[runs[0]].text) {
                continue;
            }
            let values: Vec<Values> = runs
                .iter()
                .map(|&index| values(&statements[index]))
                .collect();
            // Without the values in the log, it cannot be called a repeat.
            let unknown = values
                .iter()
                .any(|values| matches!(values, Values::Parameters(logged) if logged.is_empty()));
            let varied = unknown || values.windows(2).any(|pair| pair[0] != pair[1]);
            let key = shape.key.clone();
            let entry = match found.iter_mut().position(|other| other.key == key) {
                Some(at) => &mut found[at],
                None => {
                    found.push(Found {
                        key,
                        requests: Vec::new(),
                    });
                    found.last_mut().expect("just pushed")
                }
            };
            entry.requests.push((number, at, runs, varied));
        }
    }

    let mut loops: Vec<(Loop, Vec<(usize, usize)>)> = found
        .into_iter()
        .map(|found| {
            let kind = if found.requests.iter().any(|(_, _, _, varied)| *varied) {
                LoopKind::Loop
            } else {
                LoopKind::Repeat
            };
            // The example: the request with the most runs, of the kind.
            let example = found
                .requests
                .iter()
                .filter(|(_, _, _, varied)| *varied == (kind == LoopKind::Loop))
                .max_by_key(|(number, _, runs, _)| (runs.len(), std::cmp::Reverse(*number)))
                .expect("a loop has a request of its kind");
            let counts: Vec<usize> = found
                .requests
                .iter()
                .map(|(_, _, runs, _)| runs.len())
                .collect();
            let all: Vec<usize> = found
                .requests
                .iter()
                .flat_map(|(_, _, runs, _)| runs.iter().copied())
                .collect();
            let total = total(statements, &all);
            let mut item = build(statements, &requests[example.0], &example.2, kind);
            item.requests = found.requests.len();
            item.runs = counts.iter().sum();
            item.fewest = counts.iter().copied().min().unwrap_or(0);
            item.most = counts.iter().copied().max().unwrap_or(0);
            item.total = total;
            item.example = example.0;
            let marks = found
                .requests
                .iter()
                .map(|(number, at, _, _)| (*number, *at))
                .collect();
            (item, marks)
        })
        .collect();
    // Loops before repeats: a loop is what batching fixes.
    loops.sort_by(|(a, _), (b, _)| {
        (a.kind == LoopKind::Repeat)
            .cmp(&(b.kind == LoopKind::Repeat))
            .then_with(|| b.runs.cmp(&a.runs))
            .then_with(|| b.total.unwrap_or(0.0).total_cmp(&a.total.unwrap_or(0.0)))
    });
    for (number, (_, marks)) in loops.iter().enumerate() {
        for &(request, shape) in marks {
            requests[request].shapes[shape].in_loop = Some(number);
        }
    }
    loops.into_iter().map(|(item, _)| item).collect()
}

/// The values of a run: its parameters, or the literal values written in
/// its text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Values {
    Parameters(Vec<Option<String>>),
    Literals(Vec<String>),
}

fn values(statement: &LoggedStatement) -> Values {
    if !statement.parameters.is_empty() || params::placeholders(&statement.text).count > 0 {
        Values::Parameters(statement.parameters.clone())
    } else {
        Values::Literals(
            tokens(&statement.text)
                .iter()
                .filter(|token| token.kind == Kind::Literal)
                .map(|token| token.text.to_owned())
                .collect(),
        )
    }
}

/// A loop from its runs in the example request.
fn build(
    statements: &[LoggedStatement],
    request: &Request,
    runs: &[usize],
    kind: LoopKind,
) -> Loop {
    let first = &statements[runs[0]];
    let mut notes = Vec::new();
    let Runs {
        single,
        varying,
        values,
        unbatched,
    } = match values(first) {
        Values::Parameters(_) => {
            let count = runs
                .iter()
                .map(|&index| statements[index].parameters.len())
                .max()
                .unwrap_or(0)
                .max(params::placeholders(&first.text).count);
            let values: Vec<Vec<Option<String>>> = runs
                .iter()
                .map(|&index| {
                    let mut values = statements[index].parameters.clone();
                    values.resize(count, None);
                    values
                })
                .collect();
            let varying: Vec<usize> = (0..count)
                .filter(|&at| values.iter().any(|run| run[at] != values[0][at]))
                .map(|at| at + 1)
                .collect();
            let unbatched = runs
                .iter()
                .all(|&index| statements[index].parameters.is_empty())
                .then(|| {
                    notes.push(
                        "log_min_duration_statement and log_statement log the values of parameters in a DETAIL line, unless log_parameter_max_length is 0; auto_explain logs them from PostgreSQL 16"
                            .to_owned(),
                    );
                    "the log has no values for its parameters".to_owned()
                });
            Runs {
                single: first.text.clone(),
                varying,
                values,
                unbatched,
            }
        }
        Values::Literals(_) => literal_loop(statements, runs),
    };

    let command = command_of(&top_words(&tokens(&single))).to_owned();
    let batched = if kind == LoopKind::Repeat {
        Err("it ran with the same values every time".to_owned())
    } else if let Some(reason) = unbatched {
        Err(reason)
    } else if varying.is_empty() {
        Err("the values that change from run to run could not be told apart".to_owned())
    } else {
        batch(&single, &varying, &[])
    };
    let column = varying
        .first()
        .and_then(|&number| compared_column(&single, number));
    let at = request
        .statements
        .iter()
        .position(|&index| index == runs[0])
        .expect("the run is in its request");
    let after = request.statements[..at]
        .iter()
        .rev()
        .map(|&index| &statements[index].text)
        .find(|text| key(text) != key(&first.text) && !is_utility(text))
        .map(|text| timeline::normalize(text));
    let tags: BTreeMap<String, String> = timeline::tags(&first.text)
        .into_iter()
        .filter(|(key, _)| key != "traceparent" && key != "tracestate")
        .collect();
    let mut item = Loop {
        kind,
        text: timeline::normalize(&first.text),
        command,
        requests: 0,
        runs: 0,
        fewest: 0,
        most: 0,
        total: None,
        example: 0,
        after,
        single,
        varying,
        values,
        column,
        tags,
        batched,
        advice: Vec::new(),
        reference: None,
        proof: None,
        notes,
    };
    item.advice = advice(&item);
    item
}

/// What the runs of a loop give its batched statement.
struct Runs {
    /// The statement, with a parameter for each value that changes.
    single: String,
    /// The parameters whose values change from run to run, from 1.
    varying: Vec<usize>,
    /// Each run's values of the parameters.
    values: Vec<Vec<Option<String>>>,
    /// Why the runs cannot be batched, if they cannot.
    unbatched: Option<String>,
}

/// A loop of statements with their values written in: the statement with
/// `$1`, `$2`, … for each literal whose value changes from run to run, the
/// others kept.
fn literal_loop(statements: &[LoggedStatement], runs: &[usize]) -> Runs {
    let first = &statements[runs[0]].text;
    let all: Vec<Vec<Token>> = runs
        .iter()
        .map(|&index| tokens(&statements[index].text))
        .collect();
    let literals: Vec<Vec<&Token>> = all
        .iter()
        .map(|tokens| {
            tokens
                .iter()
                .filter(|token| token.kind == Kind::Literal)
                .collect()
        })
        .collect();
    if literals.iter().any(|run| run.len() != literals[0].len()) {
        return Runs {
            single: first.clone(),
            varying: Vec::new(),
            values: Vec::new(),
            unbatched: Some(
                "its runs have lists of different lengths (IN (…)), so the values that change cannot be lined up"
                    .to_owned(),
            ),
        };
    }
    let changing: Vec<usize> = (0..literals[0].len())
        .filter(|&at| {
            literals
                .iter()
                .any(|run| run[at].text != literals[0][at].text)
        })
        .collect();
    let mut single = String::with_capacity(first.len());
    let mut copied = 0;
    for (number, &at) in changing.iter().enumerate() {
        let token = literals[0][at];
        single.push_str(&first[copied..token.start]);
        single.push_str(&format!("${}", number + 1));
        copied = token.end();
    }
    single.push_str(&first[copied..]);
    let values: Vec<Vec<Option<String>>> = literals
        .iter()
        .map(|run| changing.iter().map(|&at| run[at].value.clone()).collect())
        .collect();
    // A string with escapes or a prefix (E'…', B'…') is not read back.
    let unbatched = values
        .iter()
        .any(|run| run.iter().any(Option::is_none))
        .then(|| {
            "a value that changes is written in a form that is not read back (E'…', B'…')"
                .to_owned()
        });
    Runs {
        single,
        varying: (1..=changing.len()).collect(),
        values,
        unbatched,
    }
}

/// The column a parameter is compared with by `=` (or `IN ($n)`), as the
/// statement writes it.
fn compared_column(sql: &str, number: usize) -> Option<String> {
    let tokens = tokens(sql);
    let at = tokens
        .iter()
        .position(|token| token.kind == Kind::Param(number))?;
    equality(&tokens, at).map(|(column, _, _)| column)
}

/// The column a parameter at `at` is compared with for equality, the
/// column's first token, and the first token of the comparison's right
/// side: `col = $1` (`=`), or `col IN ($1)` (`IN`).
fn equality(tokens: &[Token], at: usize) -> Option<(String, usize, usize)> {
    let before = |offset: usize| at.checked_sub(offset).map(|index| &tokens[index]);
    let (operator, column_end) = if before(1).is_some_and(|token| token.text == "=") {
        (at - 1, at - 1)
    } else if before(1).is_some_and(|token| token.text == "(")
        && before(2).is_some_and(|token| token.text.eq_ignore_ascii_case("in"))
        && tokens.get(at + 1).is_some_and(|token| token.text == ")")
    {
        (at - 2, at - 2)
    } else {
        return None;
    };
    // The column: a name, maybe qualified (`oi.order_id`).
    let mut start = column_end;
    let mut parts = Vec::new();
    loop {
        let token = start.checked_sub(1).map(|index| &tokens[index])?;
        if token.kind != Kind::Word || is_keyword(token.text) {
            break;
        }
        parts.push(token.text);
        start -= 1;
        if start >= 2 && tokens[start - 1].text == "." && tokens[start - 2].kind == Kind::Word {
            start -= 1;
            continue;
        }
        break;
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some((parts.join("."), start, operator))
}

/// The statement that does the work of the runs of `single` at once: each
/// parameter in `varying` takes an array of its values. `types` holds the
/// parameters' types, from `$1`, when they are known. Comments are left
/// out.
pub fn batch(single: &str, varying: &[usize], types: &[String]) -> Result<Batched, String> {
    let single = strip_comments(single);
    let single = single.as_str();
    let tokens = tokens(single);
    let words = top_words(&tokens);
    let command = command_of(&words);
    let array = |number: usize| match types.get(number - 1) {
        Some(type_name) => format!("${number}::{type_name}[]"),
        None => format!("${number}"),
    };
    if command == "insert" {
        return Err(
            "it inserts one row at a time: send the rows in one INSERT … VALUES (…), (…), or as a batch (JDBC addBatch, hibernate.jdbc.batch_size)"
                .to_owned(),
        );
    }
    if !matches!(command, "select" | "update" | "delete") {
        return Err(format!(
            "a {} statement is not batched",
            command.to_uppercase()
        ));
    }
    // Each varying parameter where it appears.
    let mut places: Vec<(usize, usize)> = Vec::new();
    for &number in varying {
        let found: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, token)| token.kind == Kind::Param(number))
            .map(|(at, _)| at)
            .collect();
        if found.is_empty() {
            return Err(format!("${number} does not appear in the statement"));
        }
        places.extend(found.into_iter().map(|at| (number, at)));
    }
    // A literal that stands for a value of a type (`date '…'`) cannot take
    // a parameter's place: a name before a value is its type.
    if places.iter().any(|&(_, at)| {
        at > 0 && tokens[at - 1].kind == Kind::Word && !is_keyword(tokens[at - 1].text)
    }) {
        return Err("a value that changes is written as a typed literal".to_owned());
    }

    // `= ANY`: one varying parameter, once, compared for equality as a term
    // of the statement's own WHERE, between its ANDs and ORs with nothing
    // applied to either side (no NOT, cast or operator), and nothing that
    // works per value. Then it finds the rows of all the runs.
    let where_at = tokens
        .iter()
        .position(|token| token.depth == 0 && token.text.eq_ignore_ascii_case("where"));
    let word_is = |at: usize, list: &[&str]| {
        tokens.get(at).is_some_and(|token| {
            token.depth == 0
                && token.kind == Kind::Word
                && list.contains(&token.text.to_ascii_lowercase().as_str())
        })
    };
    let per_value = [
        "limit",
        "offset",
        "fetch",
        "group",
        "having",
        "window",
        "over",
        "union",
        "intersect",
        "except",
    ];
    // SELECT DISTINCT, not IS DISTINCT FROM.
    let distinct = tokens.windows(2).any(|pair| {
        pair[0].depth == 0
            && pair[0].text.eq_ignore_ascii_case("select")
            && pair[1].text.eq_ignore_ascii_case("distinct")
    });
    let aggregate = has_aggregate(&tokens);
    let any = match places.as_slice() {
        [(number, at)]
            if where_at.is_some_and(|where_at| {
                where_at < *at
                    && !(where_at..*at).any(|index| word_is(index, &["order", "for", "returning"]))
            }) && !distinct
                && !words.iter().any(|word| per_value.contains(&word.as_str()))
                && !(command == "select" && aggregate) =>
        {
            equality(&tokens, *at)
                .filter(|&(_, start, operator)| {
                    // The comparison's last token: the parameter, or the
                    // parenthesis of IN ($1).
                    let end = if tokens[operator].text == "=" {
                        *at
                    } else {
                        at + 1
                    };
                    tokens[operator].depth == 0
                        && start > 0
                        && word_is(start - 1, &["where", "and", "or"])
                        && (end + 1 == tokens.len()
                            || tokens[end + 1].text == ";"
                            || word_is(end + 1, &["and", "or", "order", "for", "returning"]))
                })
                .map(|(column, _, operator)| (*number, *at, column, operator))
        }
        _ => None,
    };
    if let Some((number, at, column, operator)) = any {
        // From the `=` to the parameter, or from the IN to its parenthesis.
        let end = if tokens[operator].text == "=" {
            tokens[at].end()
        } else {
            tokens[at + 1].end()
        };
        let sql = finish(&format!(
            "{} = ANY({}){}",
            single[..tokens[operator].start].trim_end(),
            array(number),
            &single[end..]
        ));
        let selected = command != "select" || selects(&tokens, &column);
        return Ok(Batched {
            form: Form::Any,
            sql,
            note: (!selected)
                .then(|| format!("select {column} too, to tell which rows go with which value")),
        });
    }

    if command != "select" {
        return Err(
            "the value that changes is not compared for equality in its WHERE: write the batched statement by hand"
                .to_owned(),
        );
    }
    // LATERAL: the statement runs once per value, inside one statement.
    let names: Vec<String> = if varying.len() == 1 {
        vec!["value".to_owned()]
    } else {
        varying
            .iter()
            .map(|number| format!("value_{number}"))
            .collect()
    };
    let mut inner = String::new();
    let mut copied = 0;
    for token in &tokens {
        let Kind::Param(number) = token.kind else {
            continue;
        };
        let Some(position) = varying.iter().position(|&other| other == number) else {
            continue;
        };
        inner.push_str(&single[copied..token.start]);
        inner.push_str(&format!("batch.{}", names[position]));
        copied = token.end();
    }
    inner.push_str(&single[copied..]);
    let inner = finish(&inner);
    let arrays: Vec<String> = varying.iter().map(|&number| array(number)).collect();
    Ok(Batched {
        form: Form::Lateral,
        sql: format!(
            "SELECT batch.*, x.* FROM unnest({}) AS batch({}) CROSS JOIN LATERAL ({inner}) AS x",
            arrays.join(", "),
            names.join(", ")
        ),
        note: aggregate.then(|| {
            "each value gets its own row, as a run did, even where no row matched".to_owned()
        }),
    })
}

/// A statement without its comments: each one, with the blanks around it,
/// becomes one space, or nothing at its ends and next to brackets, commas
/// and semicolons, which no operator has in it.
fn strip_comments(sql: &str) -> String {
    fn append(out: &mut String, chunk: &str, space: &mut bool) {
        let chunk = if *space { chunk.trim_start() } else { chunk };
        if chunk.is_empty() {
            return;
        }
        if *space && !out.is_empty() && !out.ends_with('(') && !chunk.starts_with([';', ',', ')']) {
            out.push(' ');
        }
        *space = false;
        out.push_str(chunk);
    }
    let mut out = String::with_capacity(sql.len());
    let mut copied = 0;
    let mut space = false;
    for token in tokens(sql)
        .iter()
        .filter(|token| token.kind == Kind::Comment)
    {
        append(&mut out, sql[copied..token.start].trim_end(), &mut space);
        space = true;
        copied = token.end();
    }
    append(&mut out, &sql[copied..], &mut space);
    out
}

/// A statement without its final semicolons and blanks.
fn finish(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_owned()
}

/// Whether the top-level select list has an aggregate.
fn has_aggregate(tokens: &[Token]) -> bool {
    const AGGREGATES: [&str; 12] = [
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "array_agg",
        "string_agg",
        "json_agg",
        "jsonb_agg",
        "bool_and",
        "bool_or",
        "every",
    ];
    let mut in_list = false;
    for (at, token) in tokens.iter().enumerate() {
        if token.depth == 0 && token.kind == Kind::Word {
            let word = token.text.to_ascii_lowercase();
            if word == "select" {
                in_list = true;
                continue;
            }
            if word == "from" {
                in_list = false;
            }
        }
        if in_list
            && token.depth == 0
            && token.kind == Kind::Word
            && AGGREGATES.contains(&token.text.to_ascii_lowercase().as_str())
            && tokens.get(at + 1).is_some_and(|next| next.text == "(")
        {
            return true;
        }
    }
    false
}

/// Whether the top-level select list has the column, or `*`.
fn selects(tokens: &[Token], column: &str) -> bool {
    let name = column.rsplit('.').next().unwrap_or(column);
    let mut in_list = false;
    for token in tokens {
        if token.depth == 0 && token.kind == Kind::Word {
            match token.text.to_ascii_lowercase().as_str() {
                "select" | "returning" => {
                    in_list = true;
                    continue;
                }
                "from" | "where" => in_list = false,
                _ => {}
            }
        }
        if in_list
            && (token.text == "*"
                || (token.kind == Kind::Word && token.text.eq_ignore_ascii_case(name)))
        {
            return true;
        }
    }
    false
}

/// How to make an application run a loop as one statement, for its
/// framework (sqlcommenter's `framework` tag) when it is known: by the
/// foreign key behind it when connected mode found it.
pub fn advice(item: &Loop) -> Vec<String> {
    let (kind, command, reference) = (item.kind, item.command.as_str(), item.reference.as_ref());
    let framework = item.tags.get("framework").map(String::as_str);
    let aggregate = has_aggregate(&tokens(&item.single));
    let framework = framework.map(str::to_ascii_lowercase).unwrap_or_default();
    let mut lines: Vec<(&str, String)> = Vec::new();
    match (kind, command) {
        (LoopKind::Repeat, _) => {
            lines.push((
                "",
                "Read it once per request and keep the result, rather than asking the database again."
                    .to_owned(),
            ));
        }
        (LoopKind::Loop, "insert") => {
            lines.push((
                "jpa",
                "JPA: set hibernate.jdbc.batch_size (and hibernate.order_inserts), with a sequence rather than IDENTITY ids, which turn batching off.".to_owned(),
            ));
            lines.push(("django", "Django: bulk_create.".to_owned()));
            lines.push(("rails", "Rails: insert_all.".to_owned()));
        }
        (LoopKind::Loop, "update" | "delete") => {
            lines.push((
                "jpa",
                format!(
                    "JPA: one bulk {} … WHERE id IN :ids (a JPQL or @Modifying query), or hibernate.jdbc.batch_size to send the runs together.",
                    command.to_uppercase()
                ),
            ));
            lines.push((
                "django",
                format!(
                    "Django: queryset.{}().",
                    if command == "update" {
                        "update"
                    } else {
                        "delete"
                    }
                ),
            ));
            lines.push((
                "rails",
                format!(
                    "Rails: {}.",
                    if command == "update" {
                        "update_all"
                    } else {
                        "delete_all"
                    }
                ),
            ));
        }
        (LoopKind::Loop, _) if aggregate => {
            let column = item.column.as_deref().unwrap_or("the column");
            lines.push((
                "jpa",
                format!(
                    "JPA: one query for all of them, grouped: … WHERE {column} IN :ids GROUP BY {column}, selecting {column} with the aggregate."
                ),
            ));
            lines.push((
                "django",
                "Django: filter(…__in=ids).values(…).annotate(…).".to_owned(),
            ));
            lines.push(("rails", "Rails: where(…: ids).group(…).count.".to_owned()));
        }
        (LoopKind::Loop, _) => match reference {
            Some(reference) if reference.children => {
                lines.push((
                    "jpa",
                    format!(
                        "JPA: JOIN FETCH or an @EntityGraph on the @OneToMany collection mapped by {}.{}, or @BatchSize on it (hibernate.default_batch_fetch_size for all).",
                        reference.from_table, reference.from_column
                    ),
                ));
                lines.push(("django", "Django: prefetch_related.".to_owned()));
                lines.push(("rails", "Rails: includes (or preload).".to_owned()));
            }
            Some(reference) => {
                lines.push((
                    "jpa",
                    format!(
                        "JPA: JOIN FETCH or an @EntityGraph on the @ManyToOne mapped by {}.{}, or @BatchSize on the entity of {} (hibernate.default_batch_fetch_size for all).",
                        reference.from_table, reference.from_column, reference.to_table
                    ),
                ));
                lines.push(("django", "Django: select_related.".to_owned()));
                lines.push(("rails", "Rails: includes.".to_owned()));
            }
            None => {
                lines.push((
                    "jpa",
                    "JPA: JOIN FETCH or an @EntityGraph on the association, or @BatchSize on it (hibernate.default_batch_fetch_size for all).".to_owned(),
                ));
                lines.push((
                    "django",
                    "Django: select_related or prefetch_related.".to_owned(),
                ));
                lines.push(("rails", "Rails: includes.".to_owned()));
            }
        },
    }
    let matches = |key: &str| match key {
        "jpa" => [
            "spring",
            "hibernate",
            "jpa",
            "java",
            "jdbc",
            "micronaut",
            "quarkus",
        ]
        .iter()
        .any(|name| framework.contains(name)),
        "django" => framework.contains("django"),
        "rails" => framework.contains("rails") || framework.contains("active"),
        _ => true,
    };
    let known = lines.iter().any(|(key, _)| !key.is_empty() && matches(key));
    lines
        .into_iter()
        .filter(|(key, _)| !known || key.is_empty() || matches(key))
        .map(|(_, line)| line)
        .collect()
}

/// The foreign key that explains a loop over `table.column`: one that
/// references it (the loop reads each child's parent), or one from it (the
/// loop reads each parent's children). `keys` holds the foreign keys of a
/// single column that start or end at it; one whose other table the
/// statement before the loop reads is preferred.
pub fn reference(
    table: &str,
    column: &str,
    keys: &[Reference],
    after: Option<&str>,
) -> Option<Reference> {
    let candidates: Vec<Reference> = keys
        .iter()
        .filter_map(|key| {
            if key.from_table == table && key.from_column == column {
                Some(Reference {
                    children: true,
                    ..key.clone()
                })
            } else if key.to_table == table && key.to_column == column {
                Some(Reference {
                    children: false,
                    ..key.clone()
                })
            } else {
                None
            }
        })
        .collect();
    let reads = |name: &str| {
        after.is_some_and(|after| {
            tokens(after)
                .iter()
                .any(|token| token.kind == Kind::Word && token.text.eq_ignore_ascii_case(name))
        })
    };
    candidates
        .iter()
        .find(|key| {
            reads(if key.children {
                &key.to_table
            } else {
                &key.from_table
            })
        })
        .or(candidates.first())
        .cloned()
}

/// An array literal of values, for a parameter that takes an array:
/// `{"1","2",NULL}`.
pub fn array_literal(values: &[Option<&str>]) -> String {
    let items: Vec<String> = values
        .iter()
        .map(|value| match value {
            Some(value) => format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")),
            None => "NULL".to_owned(),
        })
        .collect();
    format!("{{{}}}", items.join(","))
}

/// What the batched statement saves against the runs it replaces, from
/// their measured plans: `single` holds the runs measured, `runs` how many
/// ran, and `batched` the batched statement's runs, the median counting.
pub fn proof(
    sql: String,
    runs: usize,
    values: usize,
    single: &[Plan],
    batched: &[Plan],
    round_trip: Option<f64>,
) -> Proof {
    let time = |plan: &Plan| {
        plan.summary
            .execution_time
            .map(|execution| execution + plan.summary.planning_time.unwrap_or(0.0))
    };
    #[allow(clippy::cast_precision_loss)]
    let scale = if single.is_empty() {
        0.0
    } else {
        runs as f64 / single.len() as f64
    };
    let single_time = single
        .iter()
        .map(time)
        .collect::<Option<Vec<f64>>>()
        .map(|times| times.iter().sum::<f64>() * scale);
    let single_pages = single
        .iter()
        .map(|plan| compare::figures_of_runs(std::slice::from_ref(plan)).pages)
        .collect::<Option<Vec<u64>>>()
        .map(|pages| {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let scaled = (pages.iter().sum::<u64>() as f64 * scale).round() as u64;
            scaled
        });
    let mut times: Vec<f64> = batched.iter().filter_map(time).collect();
    times.sort_by(f64::total_cmp);
    let batched_time = (!times.is_empty()).then(|| times[times.len() / 2]);
    let single_access = single
        .first()
        .map(|plan| params::brief(plan, &[]).access)
        .unwrap_or_default();
    let batched_access = batched
        .first()
        .map(|plan| params::brief(plan, &[]).access)
        .unwrap_or_default();
    #[allow(clippy::cast_precision_loss)]
    let saved = match (single_time, batched_time) {
        (Some(single), Some(batched)) => {
            Some(single - batched + round_trip.unwrap_or(0.0) * (runs as f64 - 1.0))
        }
        _ => None,
    };
    Proof {
        sql,
        runs,
        measured: single.len(),
        values,
        plan_changed: crate::fingerprint::blank_numbers(&single_access)
            != crate::fingerprint::blank_numbers(&batched_access),
        single: Side {
            time: single_time,
            pages: single_pages,
            access: single_access,
        },
        batched: Side {
            time: batched_time,
            pages: compare::figures_of_runs(batched).pages,
            access: batched_access,
        },
        round_trip,
        saved,
    }
}

/// A token of a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token<'a> {
    text: &'a str,
    /// Where it starts in the statement, in bytes.
    start: usize,
    /// How many parentheses it is inside.
    depth: usize,
    kind: Kind,
    /// A literal's value: a string's without its quotes, a number as
    /// written; `None` when it cannot be read back exactly.
    value: Option<String>,
}

impl Token<'_> {
    fn end(&self) -> usize {
        self.start + self.text.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Param(usize),
    Literal,
    Comment,
    Symbol,
}

/// The tokens of a statement: words, `$n` parameters, literal values,
/// comments, and symbols (operators as one token, punctuation one by one).
fn tokens(sql: &str) -> Vec<Token<'_>> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut depth = 0usize;
    let mut at = 0;
    while let Some(c) = sql[at..].chars().next() {
        let rest = &sql[at..];
        if c.is_whitespace() {
            at += c.len_utf8();
            continue;
        }
        let previous_word_ends_here = tokens
            .last()
            .is_some_and(|token| token.kind == Kind::Word && token.end() == at);
        let (length, kind, value) = if rest.starts_with("--") {
            (rest.find('\n').unwrap_or(rest.len()), Kind::Comment, None)
        } else if rest.starts_with("/*") {
            (params::block_comment(rest), Kind::Comment, None)
        } else if c == '\'' {
            // A prefix (E'…', B'…', X'…', N'…') goes with the literal; only
            // E'…' takes backslash escapes, and its value is not read back.
            let prefix = tokens.last().filter(|token| {
                previous_word_ends_here && token.text.len() == 1 && "EeBbXxNn".contains(token.text)
            });
            let escapes = prefix.is_some_and(|token| token.text.eq_ignore_ascii_case("e"));
            let length = params::quoted(rest, '\'', escapes);
            let quoted = &rest[..length];
            let value = (prefix.is_none() && quoted.len() >= 2 && quoted.ends_with('\''))
                .then(|| quoted[1..quoted.len() - 1].replace("''", "'"));
            if let Some(prefix) = prefix.cloned() {
                tokens.pop();
                let start = prefix.start;
                tokens.push(Token {
                    text: &sql[start..at + length],
                    start,
                    depth,
                    kind: Kind::Literal,
                    value: None,
                });
                at += length;
                continue;
            }
            (length, Kind::Literal, value)
        } else if c == '"' {
            (params::quoted(rest, '"', false), Kind::Word, None)
        } else if c == '$' {
            if let Some(length) = params::dollar_quote(rest) {
                let tag = rest[1..].find('$').map_or(1, |end| end + 2);
                let value = rest[tag..length - tag].to_owned();
                (length, Kind::Literal, Some(value))
            } else {
                let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
                match rest[1..=digits].parse::<usize>() {
                    Ok(number) if digits > 0 && !previous_word_ends_here => {
                        (1 + digits, Kind::Param(number), None)
                    }
                    _ => (1, Kind::Symbol, None),
                }
            }
        } else if (c.is_ascii_digit()
            || (c == '.' && rest[1..].starts_with(|c: char| c.is_ascii_digit())))
            && !previous_word_ends_here
        {
            let length = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .unwrap_or(rest.len());
            (length, Kind::Literal, Some(rest[..length].to_owned()))
        } else if c.is_alphabetic() || c == '_' {
            let length = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            (length, Kind::Word, None)
        } else if "+-*/<>=~!@#%^&|`?".contains(c) {
            let length = rest
                .find(|c: char| !"+-*/<>=~!@#%^&|`?".contains(c))
                .unwrap_or(rest.len());
            (length, Kind::Symbol, None)
        } else {
            (c.len_utf8(), Kind::Symbol, None)
        };
        if c == ')' {
            depth = depth.saturating_sub(1);
        }
        tokens.push(Token {
            text: &rest[..length],
            start: at,
            depth,
            kind,
            value,
        });
        if c == '(' {
            depth += 1;
        }
        at += length;
    }
    tokens
}

/// The statement's words outside parentheses, in lower case.
fn top_words(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .filter(|token| {
            token.depth == 0 && token.kind == Kind::Word && !token.text.starts_with('"')
        })
        .map(|token| token.text.to_ascii_lowercase())
        .collect()
}

/// A statement's command, from its top-level words: after a WITH's
/// queries, the one they are for.
fn command_of(words: &[String]) -> &str {
    match words.first().map(String::as_str) {
        Some("with") => words
            .iter()
            .map(String::as_str)
            .find(|word| matches!(*word, "select" | "insert" | "update" | "delete"))
            .unwrap_or("select"),
        Some(word) => word,
        None => "",
    }
}

/// Words that may come before a value without making it a typed literal,
/// and that are not a column's name.
fn is_keyword(word: &str) -> bool {
    const KEYWORDS: [&str; 42] = [
        "and",
        "or",
        "not",
        "in",
        "is",
        "like",
        "ilike",
        "similar",
        "between",
        "then",
        "else",
        "when",
        "case",
        "limit",
        "offset",
        "select",
        "values",
        "set",
        "return",
        "by",
        "distinct",
        "all",
        "any",
        "some",
        "to",
        "escape",
        "where",
        "on",
        "having",
        "returning",
        "from",
        "for",
        "first",
        "next",
        "zone",
        "default",
        "placing",
        "both",
        "leading",
        "trailing",
        "symmetric",
        "asymmetric",
    ];
    KEYWORDS.contains(&word.to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg::LogMeta;

    fn statement(at: &str, ms: f64, text: &str, parameters: &[&str]) -> LoggedStatement {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let duration_us = Some((ms * 1000.0) as u64);
        LoggedStatement {
            meta: LogMeta {
                timestamp: Some(format!("2026-10-06 {at} UTC")),
                pid: Some(7),
                duration_us,
                ..LogMeta::default()
            },
            text: text.to_owned(),
            prepared: None,
            parameters: parameters
                .iter()
                .map(|value| Some((*value).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn batches_with_any() {
        let batched = batch(
            "SELECT id, order_id, quantity FROM order_items WHERE order_id = $1 /*action='latest'*/",
            &[1],
            &["integer".to_owned()],
        )
        .unwrap();
        assert_eq!(batched.form, Form::Any);
        assert_eq!(
            batched.sql,
            "SELECT id, order_id, quantity FROM order_items WHERE order_id = ANY($1::integer[])"
        );
        assert_eq!(batched.note, None);
        // Without the column in the select list, and with IN ($1).
        let batched = batch(
            "select i.id from order_items i where i.order_id in ($1) and i.quantity > $2;",
            &[1],
            &[],
        )
        .unwrap();
        assert_eq!(
            batched.sql,
            "select i.id from order_items i where i.order_id = ANY($1) and i.quantity > $2"
        );
        assert_eq!(
            batched.note.as_deref(),
            Some("select i.order_id too, to tell which rows go with which value")
        );
        // As Hibernate writes it.
        let batched = batch(
            "select o1_0.id,o1_0.order_id from order_items o1_0 where o1_0.order_id=$1",
            &[1],
            &[],
        )
        .unwrap();
        assert_eq!(
            batched.sql,
            "select o1_0.id,o1_0.order_id from order_items o1_0 where o1_0.order_id = ANY($1)"
        );
        assert_eq!(batched.note, None);
        // A write.
        let batched = batch("UPDATE orders SET status = $1 WHERE id = $2", &[2], &[]).unwrap();
        assert_eq!(
            batched.sql,
            "UPDATE orders SET status = $1 WHERE id = ANY($2)"
        );
        assert!(batch("UPDATE orders SET status = $1 WHERE id = $2", &[1], &[]).is_err());
        assert!(
            batch("INSERT INTO t (a) VALUES ($1)", &[1], &[])
                .unwrap_err()
                .contains("one INSERT")
        );
    }

    #[test]
    fn batches_with_lateral_what_works_per_value() {
        let batched = batch(
            "SELECT count(*) FROM orders WHERE customer_id = $1;",
            &[1],
            &["integer".to_owned()],
        )
        .unwrap();
        assert_eq!(batched.form, Form::Lateral);
        assert_eq!(
            batched.sql,
            "SELECT batch.*, x.* FROM unnest($1::integer[]) AS batch(value) CROSS JOIN LATERAL (SELECT count(*) FROM orders WHERE customer_id = batch.value) AS x"
        );
        assert!(batched.note.is_some());
        let batched = batch(
            "SELECT id FROM orders WHERE customer_id = $1 AND status = $2 ORDER BY created_at DESC LIMIT 5",
            &[1, 2],
            &[],
        )
        .unwrap();
        assert_eq!(
            batched.sql,
            "SELECT batch.*, x.* FROM unnest($1, $2) AS batch(value_1, value_2) CROSS JOIN LATERAL (SELECT id FROM orders WHERE customer_id = batch.value_1 AND status = batch.value_2 ORDER BY created_at DESC LIMIT 5) AS x"
        );
        // A typed literal cannot take a parameter.
        assert!(batch("SELECT 1 FROM t WHERE d = date $1", &[1], &[]).is_err());
    }

    #[test]
    fn reads_tokens() {
        let tokens =
            tokens("SELECT E'a\\'b', 'it''s', $$x$$, 1.5, t1.c FROM t WHERE x = $2 -- c\n");
        let literals: Vec<(&str, Option<&str>)> = tokens
            .iter()
            .filter(|token| token.kind == Kind::Literal)
            .map(|token| (token.text, token.value.as_deref()))
            .collect();
        assert_eq!(
            literals,
            vec![
                ("E'a\\'b'", None),
                ("'it''s'", Some("it's")),
                ("$$x$$", Some("x")),
                ("1.5", Some("1.5"))
            ]
        );
        assert!(tokens.iter().any(|token| token.kind == Kind::Param(2)));
        assert!(
            !tokens
                .iter()
                .any(|token| token.text == "1" && token.kind == Kind::Literal)
        );
        assert_eq!(
            array_literal(&[Some("1"), None, Some("a\"b\\")]),
            "{\"1\",NULL,\"a\\\"b\\\\\"}"
        );
    }

    #[test]
    fn groups_by_session_and_gaps() {
        let statements = vec![
            statement("06:00:00.010", 1.0, "SELECT a FROM t WHERE id = 1", &[]),
            statement("06:00:00.012", 1.0, "SELECT a FROM t WHERE id = 2", &[]),
            statement("06:00:00.014", 1.0, "SELECT a FROM t WHERE id = 3", &[]),
            // Idle for longer than the gap: another request.
            statement("06:00:01.000", 1.0, "SELECT a FROM t WHERE id = 4", &[]),
        ];
        let profile = profile(&statements, Options::default());
        assert_eq!(profile.requests.len(), 2);
        assert_eq!(profile.requests[0].statements, vec![0, 1, 2]);
        assert_eq!(profile.requests[0].by, Grouping::Session);
        assert_eq!(profile.loops.len(), 1);
        let found = &profile.loops[0];
        assert_eq!(found.kind, LoopKind::Loop);
        assert_eq!(found.single, "SELECT a FROM t WHERE id = $1");
        assert_eq!(found.varying, vec![1]);
        assert_eq!(found.column.as_deref(), Some("id"));
        assert_eq!(
            found.values,
            vec![
                vec![Some("1".to_owned())],
                vec![Some("2".to_owned())],
                vec![Some("3".to_owned())]
            ]
        );
        assert_eq!(profile.requests[0].shapes[0].in_loop, Some(0));
        assert_eq!(profile.requests[1].shapes[0].in_loop, None);
    }

    #[test]
    fn tells_repeats_from_loops() {
        let statements: Vec<LoggedStatement> = (0..4)
            .map(|n| {
                statement(
                    &format!("06:00:00.0{n}0"),
                    0.1,
                    "SELECT v FROM kv WHERE k = $1",
                    &["key7"],
                )
            })
            .collect();
        let profile = profile(&statements, Options::default());
        assert_eq!(profile.loops[0].kind, LoopKind::Repeat);
        assert!(profile.loops[0].batched.is_err());
        // Fewer runs than the least that make a loop.
        let few = profile_of(&statements[..2]);
        assert!(few.loops.is_empty());
    }

    fn profile_of(statements: &[LoggedStatement]) -> Profile {
        profile(statements, Options::default())
    }

    #[test]
    fn finds_the_foreign_key() {
        let key = Reference {
            constraint: "order_items_order_id_fkey".to_owned(),
            from_table: "order_items".to_owned(),
            from_column: "order_id".to_owned(),
            to_table: "orders".to_owned(),
            to_column: "id".to_owned(),
            children: false,
        };
        let found = reference("order_items", "order_id", std::slice::from_ref(&key), None).unwrap();
        assert!(found.children);
        let found = reference("orders", "id", &[key], None).unwrap();
        assert!(!found.children);
        // The advice for it, for the application's framework.
        let statements: Vec<LoggedStatement> = (1..=3)
            .map(|n| {
                statement(
                    &format!("06:00:00.0{n}0"),
                    0.1,
                    &format!("SELECT id FROM orders WHERE id = {n} /*framework='spring'*/"),
                    &[],
                )
            })
            .collect();
        let mut item = profile_of(&statements).loops.remove(0);
        assert_eq!(advice(&item).len(), 1);
        item.reference = Some(found);
        let advice = advice(&item);
        assert_eq!(advice.len(), 1);
        assert!(advice[0].starts_with("JPA:"), "{advice:?}");
        assert!(advice[0].contains("@ManyToOne mapped by order_items.order_id"));
        item.tags.clear();
        assert_eq!(super::advice(&item).len(), 3);
    }

    #[test]
    fn batches_carefully() {
        // Comments go, without gluing words together.
        assert_eq!(
            strip_comments("SELECT a/* x */FROM t -- y\nWHERE id = $1 /*tags*/;"),
            "SELECT a FROM t WHERE id = $1;"
        );
        // A cast after the parameter: per value.
        let batched = batch("SELECT a FROM t WHERE id = $1::uuid", &[1], &[]).unwrap();
        assert_eq!(batched.form, Form::Lateral);
        assert!(
            batched.sql.contains("WHERE id = batch.value::uuid"),
            "{}",
            batched.sql
        );
        // A window function numbers the rows of each value.
        let batched = batch(
            "SELECT id, row_number() OVER (ORDER BY id) FROM t WHERE k = $1",
            &[1],
            &[],
        )
        .unwrap();
        assert_eq!(batched.form, Form::Lateral);
        // A value compared by a function of the column, in a write, or in
        // the write of a WITH.
        assert!(batch("DELETE FROM t WHERE lower(k) = $1", &[1], &[]).is_err());
        assert!(
            batch(
                "WITH d AS (SELECT 1) DELETE FROM t WHERE lower(k) = $1",
                &[1],
                &[]
            )
            .is_err()
        );
        // = ANY only for a term of the WHERE: not negated, nothing applied
        // to the value, not in the ORDER BY, without SELECT DISTINCT.
        for single in [
            "SELECT a FROM t WHERE NOT k = $1",
            "SELECT a FROM t WHERE k NOT IN ($1)",
            "SELECT a FROM t WHERE k = $1 + 1",
            "SELECT a FROM t WHERE k = $1 COLLATE \"C\"",
            "SELECT a FROM t WHERE x = 1 ORDER BY k = $1",
            "SELECT DISTINCT a FROM t WHERE k = $1",
        ] {
            let batched = batch(single, &[1], &[]).unwrap();
            assert_eq!(batched.form, Form::Lateral, "{single}");
        }
        let batched = batch(
            "SELECT a, k FROM t WHERE a IS DISTINCT FROM b AND k = $1 OR c ORDER BY a",
            &[1],
            &[],
        )
        .unwrap();
        assert_eq!(
            batched.sql,
            "SELECT a, k FROM t WHERE a IS DISTINCT FROM b AND k = ANY($1) OR c ORDER BY a"
        );
        // A FETCH FIRST parameter that does not change.
        let batched = batch(
            "SELECT id FROM t WHERE k = $1 ORDER BY id FETCH FIRST $2 ROWS ONLY",
            &[1],
            &[],
        )
        .unwrap();
        assert_eq!(batched.form, Form::Lateral);
    }

    #[test]
    fn calls_runs_without_values_a_loop() {
        // log_statement without the values: not a repeat, and not batched.
        let statements: Vec<LoggedStatement> = (0..3)
            .map(|n| {
                statement(
                    &format!("06:00:00.0{n}0"),
                    0.1,
                    "SELECT v FROM kv WHERE k = $1",
                    &[],
                )
            })
            .collect();
        let found = &profile_of(&statements).loops[0];
        assert_eq!(found.kind, LoopKind::Loop);
        assert_eq!(
            found.batched,
            Err("the log has no values for its parameters".to_owned())
        );
        // Values written as escaped strings are not read back.
        let statements: Vec<LoggedStatement> = (0..3)
            .map(|n| {
                statement(
                    &format!("06:00:00.0{n}0"),
                    0.1,
                    &format!("SELECT v FROM kv WHERE k = E'a\\\\{n}'"),
                    &[],
                )
            })
            .collect();
        let found = &profile_of(&statements).loops[0];
        assert_eq!(found.kind, LoopKind::Loop);
        assert!(
            found
                .batched
                .as_ref()
                .unwrap_err()
                .contains("not read back")
        );
    }
}
