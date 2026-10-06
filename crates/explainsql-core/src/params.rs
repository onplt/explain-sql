//! Statements with parameters, as applications send them: `$1`, or `?` in
//! JDBC. PostgreSQL plans such a statement for its values (a custom plan)
//! or once for any value (the generic plan). A prepared statement gets
//! custom plans for its first five executions; after that PostgreSQL uses
//! the generic plan when it estimates it cheaper than the custom plans were
//! on average, and keeps it. pgJDBC prepares a statement on the server from
//! its fifth execution (`prepareThreshold`), so in a Java application a
//! statement that runs often can end up with the generic plan. For a column
//! whose values are skewed, or a LIMIT the planner cannot see, that plan
//! can suit some values and ruin others, while the same statement with
//! literal values, as tried in psql, gets a custom plan.
//!
//! This module maps each parameter to the column it is compared with, or to
//! the LIMIT or OFFSET it counts rows for, picks values from the column's
//! statistics or common row counts, and compares the plans the values get
//! with the generic plan. Values are tried one parameter at a time, the
//! others held at a typical value: how columns depend on each other is not
//! taken into account.

use serde::Serialize;

use crate::compare::{Change, Comparison};
use crate::expr::{self, Operand};
use crate::fingerprint;
use crate::format;
use crate::ir::{Node, Plan, PredicateKind};

/// A statement's placeholders, as PostgreSQL numbers them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholders {
    /// The statement with `$1`, `$2`, …
    pub sql: String,
    /// How many parameters it takes: the highest `$n`.
    pub count: usize,
    /// Whether JDBC's `?` placeholders were turned into `$n`.
    pub converted: bool,
}

/// Reads a statement's parameters. A statement with `$n` placeholders is
/// left as it is. Otherwise each `?` outside string literals, quoted
/// identifiers, dollar quotes and comments becomes `$1`, `$2`, …, and `??`,
/// JDBC's escape for the `?` operator, becomes `?`.
pub fn placeholders(sql: &str) -> Placeholders {
    let dollars = scan(sql, false, |_| None).1;
    if dollars > 0 {
        return Placeholders {
            sql: sql.to_owned(),
            count: dollars,
            converted: false,
        };
    }
    let mut number = 0;
    let (converted, _) = scan(sql, false, |rest| {
        if rest.starts_with("??") {
            Some(("?".to_owned(), 2))
        } else if rest.starts_with('?') {
            number += 1;
            Some((format!("${number}"), 1))
        } else {
            None
        }
    });
    Placeholders {
        sql: converted,
        count: number,
        converted: number > 0,
    }
}

/// Copies `sql`, letting `replace` rewrite the text outside literals,
/// identifiers and comments, and returns the copy with the highest `$n`.
/// With `mask`, each literal, identifier and comment becomes a blank.
fn scan(
    sql: &str,
    mask: bool,
    mut replace: impl FnMut(&str) -> Option<(String, usize)>,
) -> (String, usize) {
    let mut out = String::with_capacity(sql.len());
    let mut highest = 0;
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        // Text that is copied as it is: literals, identifiers, comments.
        let skip = if c == '\'' {
            // E'…' strings take backslash escapes.
            let escapes = out.ends_with(['E', 'e']);
            quoted(rest, '\'', escapes)
        } else if c == '"' {
            quoted(rest, '"', false)
        } else if rest.starts_with("--") {
            rest.find('\n').unwrap_or(rest.len())
        } else if rest.starts_with("/*") {
            block_comment(rest)
        } else if c == '$' {
            match dollar_quote(rest) {
                Some(end) => end,
                None => {
                    let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
                    if digits > 0 && !out.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                        highest = highest.max(rest[1..=digits].parse().unwrap_or(0));
                    }
                    0
                }
            }
        } else {
            0
        };
        if skip > 0 {
            out.push_str(if mask { " " } else { &rest[..skip] });
            rest = &rest[skip..];
            continue;
        }
        if let Some((text, length)) = replace(rest) {
            out.push_str(&text);
            rest = &rest[length..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, highest)
}

/// The length of a quoted literal or identifier at the start of `text`,
/// doubled quotes included; to the end of the text when it is not closed.
pub(crate) fn quoted(text: &str, quote: char, escapes: bool) -> usize {
    let mut chars = text.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        if escapes && c == '\\' {
            chars.next();
        } else if c == quote {
            if text[at + 1..].starts_with(quote) {
                chars.next();
            } else {
                return at + 1;
            }
        }
    }
    text.len()
}

/// The length of a block comment, which may nest.
pub(crate) fn block_comment(text: &str) -> usize {
    let mut depth = 0;
    let mut at = 0;
    while at < text.len() {
        if text[at..].starts_with("/*") {
            depth += 1;
            at += 2;
        } else if text[at..].starts_with("*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    text.len()
}

/// The length of a dollar-quoted string (`$$…$$`, `$tag$…$tag$`) at the
/// start of `text`, if one starts there.
pub(crate) fn dollar_quote(text: &str) -> Option<usize> {
    let tag_length = text[1..].find('$').filter(|&end| {
        let tag = &text[1..=end];
        tag.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !tag.starts_with(|c: char| c.is_ascii_digit())
    })?;
    let tag = &text[..tag_length + 2];
    let close = text[tag.len()..].find(tag)?;
    Some(tag.len() + close + tag.len())
}

/// The statement's words in capitals, its `$n` parameters, and its other
/// characters one by one, leaving out literals, quoted identifiers and
/// comments.
fn words(sql: &str) -> Vec<String> {
    let (masked, _) = scan(sql, true, |_| None);
    let mut words = Vec::new();
    let mut rest = masked.as_str();
    while let Some(c) = rest.chars().next() {
        let length = if c.is_alphabetic() || c == '_' {
            rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len())
        } else if c == '$' {
            1 + rest[1..]
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len() - 1)
        } else {
            c.len_utf8()
        };
        if !c.is_whitespace() {
            words.push(rest[..length].to_uppercase());
        }
        rest = &rest[length..];
    }
    words
}

/// The parameters that count rows: `LIMIT $1`, `OFFSET $2`, `FETCH FIRST
/// $3 ROWS ONLY`.
fn clauses(sql: &str) -> Vec<(usize, Clause)> {
    let words = words(sql);
    let word = |at: usize| words.get(at).map(String::as_str);
    let mut found = Vec::new();
    for at in 0..words.len() {
        let (clause, mut next) = match word(at) {
            Some("LIMIT") => (Clause::Limit, at + 1),
            Some("OFFSET") => (Clause::Offset, at + 1),
            Some("FETCH") if matches!(word(at + 1), Some("FIRST" | "NEXT")) => {
                (Clause::Limit, at + 2)
            }
            _ => continue,
        };
        while word(next) == Some("(") {
            next += 1;
        }
        if let Some(number) = word(next)
            .and_then(|word| word.strip_prefix('$'))
            .and_then(|number| number.parse().ok())
        {
            found.push((number, clause));
        }
    }
    found
}

/// A parameter and what the statement does with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Parameter {
    /// `1` for `$1`.
    pub number: usize,
    /// Its type, as PostgreSQL inferred it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// The column it is compared with, if one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<ColumnUse>,
    /// The clause it counts rows for, if it is a LIMIT or an OFFSET.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clause: Option<Clause>,
    /// The value it is held at while the others are tried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// Whether that value was given rather than picked; a given value is
    /// the only one tried.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub given: bool,
}

impl Parameter {
    fn new(number: usize, type_name: Option<String>) -> Self {
        Parameter {
            number,
            type_name,
            column: None,
            clause: None,
            held: None,
            given: false,
        }
    }

    /// What the statement does with the parameter: `orders.status =`,
    /// `LIMIT`.
    pub fn role(&self) -> Option<String> {
        match (&self.column, self.clause) {
            (Some(column), _) => Some(format!("{} {}", column.name(), column.operator)),
            (None, Some(clause)) => Some(clause.keyword().to_owned()),
            (None, None) => None,
        }
    }

    /// `$1 = 'pending'`, `$2 = 10`.
    pub fn show(&self, value: Option<&str>) -> String {
        format!(
            "${} = {}",
            self.number,
            show_typed(value, self.type_name.as_deref())
        )
    }
}

/// A column a parameter is compared with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnUse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The table the plan scans: a partition, for a partitioned table.
    pub table: String,
    /// The partitioned table, when the values come from its statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitioned: Option<String>,
    pub column: String,
    /// With the column on the left: `=`, `>=`, `~~`.
    pub operator: String,
}

impl ColumnUse {
    /// `orders.status`; for a partition, the partitioned table's name when
    /// it is known.
    pub fn name(&self) -> String {
        format!(
            "{}.{}",
            self.partitioned.as_deref().unwrap_or(&self.table),
            self.column
        )
    }
}

/// A clause that counts rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Clause {
    /// `LIMIT`, or `FETCH FIRST … ROWS ONLY`.
    Limit,
    Offset,
}

impl Clause {
    pub fn keyword(self) -> &'static str {
        match self {
            Clause::Limit => "LIMIT",
            Clause::Offset => "OFFSET",
        }
    }
}

/// The parameters of a statement (`sql`, with `$n` placeholders), from a
/// plan of it that keeps them, such as the generic plan: each with the
/// column it is compared with in a scan's conditions, or the clause it
/// counts rows for. `types` holds their types, from the first.
pub fn parameters(plan: &Plan, sql: &str, types: &[String]) -> Vec<Parameter> {
    let count = types.len().max(scan(sql, false, |_| None).1);
    let mut parameters: Vec<Parameter> = (1..=count)
        .map(|number| Parameter::new(number, types.get(number - 1).cloned()))
        .collect();
    for (number, clause) in clauses(sql) {
        if let Some(parameter) = parameters.get_mut(number.wrapping_sub(1)) {
            parameter.clause.get_or_insert(clause);
        }
    }
    for node in &plan.nodes {
        let Some(table) = node.relation_name.as_deref() else {
            continue;
        };
        for kind in [
            PredicateKind::IndexCond,
            PredicateKind::RecheckCond,
            PredicateKind::Filter,
        ] {
            let Some(condition) = node.predicate(kind) else {
                continue;
            };
            for conjunct in expr::conjuncts(condition) {
                for (number, column, operator) in compared(conjunct) {
                    let Some(parameter) = parameters.get_mut(number.wrapping_sub(1)) else {
                        continue;
                    };
                    parameter.column.get_or_insert_with(|| ColumnUse {
                        schema: node.schema.clone(),
                        table: table.to_owned(),
                        partitioned: None,
                        column: expr::split_column(column).1.to_owned(),
                        operator,
                    });
                }
            }
        }
    }
    parameters
}

/// The parameters a conjunct compares a column with: each parameter's
/// number, the column and the operator with the column on the left. One,
/// or those of an IN list (`= ANY (ARRAY[$1, $2])`).
fn compared(conjunct: &str) -> Vec<(usize, &str, String)> {
    let Some(comparison) = expr::comparison(conjunct) else {
        return Vec::new();
    };
    // A cast of the column, as of a varchar to text, keeps its statistics.
    let (column, value, flipped) = match (
        expr::operand(comparison.left),
        expr::operand(comparison.right),
    ) {
        (Operand::Column(column) | Operand::Cast(column), Operand::Value) => {
            (column, comparison.right, false)
        }
        (Operand::Value, Operand::Column(column) | Operand::Cast(column)) => {
            (column, comparison.left, true)
        }
        _ => return Vec::new(),
    };
    let operator = match (flipped, comparison.operator) {
        (true, "<") => ">",
        (true, "<=") => ">=",
        (true, ">") => "<",
        (true, ">=") => "<=",
        (_, operator) => operator,
    };
    let numbers: Vec<usize> = match expr::strip_parens(value)
        .strip_prefix("ANY (ARRAY[")
        .and_then(|items| items.strip_suffix("])"))
    {
        Some(items) => expr::split(items, ", ")
            .into_iter()
            .filter_map(parameter_number)
            .collect(),
        None => parameter_number(value).into_iter().collect(),
    };
    numbers
        .into_iter()
        .map(|number| (number, column, operator.to_owned()))
        .collect()
}

/// `3` for `$3`, `($3)::text` or `$3::integer`.
fn parameter_number(value: &str) -> Option<usize> {
    let value = expr::strip_parens(value);
    let value = value.split("::").next().unwrap_or(value);
    let value = expr::strip_parens(value);
    value.strip_prefix('$')?.parse().ok()
}

/// What `pg_stats` says about a column.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnStats {
    /// The table the statistics are of: for a partition, its partitioned
    /// table, when that has them.
    pub table: String,
    pub null_frac: f64,
    /// Distinct values: a count, or minus a fraction of the rows.
    pub n_distinct: f64,
    /// The most common values, most common first, and their frequencies.
    pub common_values: Vec<String>,
    pub common_freqs: Vec<f64>,
    /// Bounds that split the other values into groups of equal size.
    pub histogram: Vec<String>,
}

/// A value to try for a parameter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    /// `None` for NULL.
    pub value: Option<String>,
    /// The share of the table's rows the condition keeps with this value,
    /// from the statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
    pub kind: SampleKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    /// Among the most common values.
    Common,
    /// The least common of the most common values.
    LeastCommon,
    /// A value outside the list of the most common ones.
    Uncommon,
    /// A bound of the histogram, for a range: from its start to its end.
    Percentile(u8),
    /// A row count for a LIMIT or an OFFSET.
    RowCount,
}

impl Sample {
    /// Where the value comes from, and the share of rows it keeps:
    /// `most common, 70% of rows`.
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            SampleKind::Common => "most common".to_owned(),
            SampleKind::LeastCommon => "least common of the most common".to_owned(),
            SampleKind::Uncommon => "not among the most common".to_owned(),
            SampleKind::Percentile(percent) => format!("{percent}th percentile"),
            SampleKind::RowCount => "row count".to_owned(),
        };
        match self.share {
            Some(share) => format!("{kind}, {} of rows", format::percent(share)),
            None => kind,
        }
    }
}

/// How many values to try for a parameter.
const MAX_SAMPLES: usize = 5;

/// The values to try for a parameter compared with a column by `operator`:
/// for equality, the most common values, the least common of them and a
/// value outside them; for a range, bounds from across the histogram.
pub fn samples(stats: &ColumnStats, operator: &str) -> Vec<Sample> {
    let rows = 1.0 - stats.null_frac;
    let histogram_share = (rows - stats.common_freqs.iter().sum::<f64>()).max(0.0);
    let mut samples = Vec::new();
    match operator {
        "<" | "<=" | ">" | ">=" => {
            let bounds = stats.histogram.len();
            if bounds >= 2 {
                for percent in [0u8, 25, 50, 75, 100] {
                    let index = (usize::from(percent) * (bounds - 1)).div_ceil(100);
                    let below = index as f64 / (bounds - 1) as f64;
                    let share = if operator.starts_with('<') {
                        below
                    } else {
                        1.0 - below
                    } * histogram_share;
                    let value = stats.histogram[index].clone();
                    if !samples
                        .iter()
                        .any(|sample: &Sample| sample.value.as_deref() == Some(&value))
                    {
                        samples.push(Sample {
                            value: Some(value),
                            share: Some(share),
                            kind: SampleKind::Percentile(percent),
                        });
                    }
                }
            }
        }
        _ => {
            let common = stats.common_values.len().min(stats.common_freqs.len());
            for index in 0..common.min(2) {
                samples.push(Sample {
                    value: Some(stats.common_values[index].clone()),
                    share: Some(stats.common_freqs[index]),
                    kind: SampleKind::Common,
                });
            }
            if common > 2 {
                samples.push(Sample {
                    value: Some(stats.common_values[common - 1].clone()),
                    share: Some(stats.common_freqs[common - 1]),
                    kind: SampleKind::LeastCommon,
                });
            }
            // A value outside the most common ones, from the middle of the
            // histogram, which covers the others.
            if let Some(value) = stats.histogram.get(stats.histogram.len() / 2) {
                if !stats.common_values.contains(value) {
                    let distinct = if stats.n_distinct < 0.0 {
                        None
                    } else {
                        Some(stats.n_distinct - common as f64)
                    };
                    samples.push(Sample {
                        value: Some(value.clone()),
                        share: distinct
                            .filter(|&others| others >= 1.0)
                            .map(|others| histogram_share / others),
                        kind: SampleKind::Uncommon,
                    });
                }
            }
        }
    }
    samples.truncate(MAX_SAMPLES);
    samples
}

/// Row counts to try for a LIMIT: one row, a page, and more.
const LIMITS: [&str; 5] = ["1", "10", "100", "1000", "10000"];
/// A page: the LIMIT held while other parameters are tried.
const PAGE: &str = "10";
/// Row counts to try for an OFFSET: the first page, and further.
const OFFSETS: [&str; 3] = ["0", "1000", "100000"];

/// The values to try for a parameter that counts rows.
pub fn row_counts(clause: Clause) -> Vec<Sample> {
    let counts: &[&str] = match clause {
        Clause::Limit => &LIMITS,
        Clause::Offset => &OFFSETS,
    };
    counts
        .iter()
        .map(|count| Sample {
            value: Some((*count).to_owned()),
            share: None,
            kind: SampleKind::RowCount,
        })
        .collect()
}

/// Sets the value each parameter is held at while the others are tried:
/// the value given for it, if any; else for a LIMIT a page of 10 rows, for
/// an OFFSET the first page, and for a column the value that keeps the
/// most rows, the most common value or the end of a range that keeps them
/// all. A parameter with nothing to try gets no value.
pub fn hold(parameters: &mut [Parameter], samples: &[Vec<Sample>], given: &[Option<String>]) {
    for (index, parameter) in parameters.iter_mut().enumerate() {
        if let Some(value) = given.get(index).cloned().flatten() {
            parameter.held = Some(value);
            parameter.given = true;
            continue;
        }
        parameter.given = false;
        parameter.held = match parameter.clause {
            Some(Clause::Limit) => Some(PAGE.to_owned()),
            Some(Clause::Offset) => Some(OFFSETS[0].to_owned()),
            None => samples
                .get(index)
                .into_iter()
                .flatten()
                .enumerate()
                // The largest share, and of equal shares the first.
                .max_by(|(a_at, a), (b_at, b)| {
                    let share = |sample: &Sample| sample.share.unwrap_or(0.0);
                    share(a).total_cmp(&share(b)).then(b_at.cmp(a_at))
                })
                .and_then(|(_, sample)| sample.value.clone()),
        };
    }
}

/// The values to run the statement with: each sample of each parameter
/// whose value was not given, with the others at the value they are held
/// at; when every value was given, those. None when a parameter has no
/// value to hold.
pub fn trials(parameters: &[Parameter], samples: &[Vec<Sample>]) -> Vec<Trial> {
    if parameters.iter().any(|parameter| parameter.held.is_none()) {
        return Vec::new();
    }
    let held = held_values(parameters);
    let mut trials = Vec::new();
    for (index, parameter) in parameters.iter().enumerate() {
        if parameter.given {
            continue;
        }
        for sample in samples.get(index).into_iter().flatten() {
            let mut values = held.clone();
            values[index] = sample.value.clone();
            trials.push(Trial {
                parameter: Some(parameter.number),
                sample: Some(sample.clone()),
                values,
            });
        }
    }
    if trials.is_empty() && parameters.iter().all(|parameter| parameter.given) {
        trials.push(Trial {
            parameter: None,
            sample: None,
            values: held,
        });
    }
    trials
}

/// One run of the statement: the parameter whose value is tried, and the
/// values of all the parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trial {
    /// The parameter whose value is tried; none for the values given.
    pub parameter: Option<usize>,
    pub sample: Option<Sample>,
    pub values: Vec<Option<String>>,
}

/// What a trial got.
#[derive(Debug, Clone)]
pub struct Tried {
    pub trial: Trial,
    /// The custom plan, for the trial's values.
    pub custom: Plan,
    /// The generic plan as it starts with the trial's values, without the
    /// partitions they rule out.
    pub generic: Plan,
    /// The custom plan before, the generic plan after, both measured with
    /// the trial's values.
    pub measured: Option<Comparison>,
    /// The timeout that stopped the generic plan, which the custom plan
    /// finished within: `30 s`.
    pub timed_out: Option<String>,
}

/// Whether the planner proved from the values alone that there is no row:
/// the plan is a Result whose one-time filter is false.
pub fn proves_empty(plan: &Plan) -> bool {
    let root = plan.root();
    plan.nodes.len() == 1
        && root.node_type == "Result"
        && root.predicate(PredicateKind::OneTimeFilter) == Some("false")
}

/// Whether a custom plan is the generic plan: the same shape, once the
/// generic plan has pruned the partitions the values rule out.
pub fn same_plan(custom: &Plan, generic: &Plan) -> bool {
    fingerprint::pruned_shape(custom) == fingerprint::pruned_shape(generic)
}

/// Whether the statement's plan depends on its parameters' values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every value tried gets the generic plan.
    Insensitive,
    /// Some values get another plan, and the generic plan is worse for
    /// them, measured or, when only estimated, as the planner sees it.
    Sensitive,
    /// Some values get another plan, but the generic plan, measured, is no
    /// worse for them.
    Harmless,
    /// No value to try.
    Unknown,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Insensitive => "INSENSITIVE",
            Verdict::Sensitive => "SENSITIVE",
            Verdict::Harmless => "HARMLESS",
            Verdict::Unknown => "UNKNOWN",
        }
    }
}

/// Which plan PostgreSQL would likely use after five executions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Likely {
    /// The generic plan, whatever the values of the first five executions.
    Generic,
    /// Either, depending on the values of the first five executions.
    Either,
    /// A custom plan for each execution.
    Custom,
}

/// Whether PostgreSQL would switch to the generic plan, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Switch {
    pub likely: Likely,
    /// One sentence.
    pub reason: String,
}

/// How the statement's plan depends on its parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sensitivity {
    pub verdict: Verdict,
    /// One sentence.
    pub summary: String,
    pub parameters: Vec<Parameter>,
    /// How the generic plan reads, and its estimated cost.
    pub generic: Brief,
    /// Which plan the report shows, and with which values.
    pub shown: String,
    pub rows: Vec<Row>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch: Option<Switch>,
    /// What to do.
    pub advice: Vec<String>,
    /// What the method leaves out, and what could not be tried.
    pub notes: Vec<String>,
    /// Whether JDBC placeholders were turned into `$n`.
    pub converted: bool,
}

/// A plan in brief: its shape, how it reads the parameters' tables, and
/// its estimated cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Brief {
    pub shape: String,
    pub access: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// One value tried.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// The parameter whose value is tried; none for the values given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter: Option<usize>,
    #[serde(flatten)]
    pub sample: Option<Sample>,
    /// The values of all the parameters.
    pub values: Vec<Option<String>>,
    /// The custom plan.
    pub plan: Brief,
    /// Whether the values get the generic plan.
    pub generic: bool,
    /// Whether the planner proves, from the values alone, that there is no
    /// row: nothing to compare.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub empty: bool,
    /// The custom plan before, the generic plan after, both measured with
    /// these values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<Comparison>,
    /// The timeout that stopped the generic plan, which the custom plan
    /// finished within.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<String>,
}

impl Row {
    /// Whether the generic plan did much worse than the custom plan,
    /// measured: it ran past the timeout, or read or took at least twice as
    /// much.
    pub fn hurt(&self) -> bool {
        self.timed_out.is_some()
            || (self
                .measured
                .as_ref()
                .is_some_and(|comparison| comparison.change == Change::Worse)
                && worse_by(self) >= MUCH_WORSE)
    }
}

/// How many times the pages, or the time, of the custom plan the generic
/// plan must take to do much worse. Less is not worth planning every
/// execution for.
const MUCH_WORSE: f64 = 2.0;

/// The planner's estimate of what planning costs, which PostgreSQL adds to
/// a custom plan's cost when it weighs it against the generic plan: 1,000
/// times `cpu_operator_cost` (0.0025 by default) per relation, plus one.
const PLANNING_COST_PER_RELATION: f64 = 2.5;

/// Compares the custom plans of the values tried with the generic plan.
/// `generic` is the generic plan the report shows, run with the values the
/// parameters are held at.
pub fn sensitivity(
    generic: &Plan,
    parameters: Vec<Parameter>,
    tried: Vec<Tried>,
    converted: bool,
) -> Sensitivity {
    let tables: Vec<String> = parameters
        .iter()
        .filter_map(|parameter| parameter.column.as_ref())
        .map(|column| column.table.clone())
        .collect();
    let generic_brief = brief(generic, &tables);
    // What each custom plan costs as PostgreSQL weighs it: with planning.
    // Values that select nothing by their own logic are left out.
    let custom_costs: Vec<f64> = tried
        .iter()
        .filter(|tried| !proves_empty(&tried.custom))
        .filter_map(|tried| {
            let relations = tried
                .custom
                .nodes
                .iter()
                .filter(|node| node.relation_name.is_some())
                .count();
            crate::compare::planner_cost(&tried.custom)
                .map(|cost| cost + PLANNING_COST_PER_RELATION * (relations as f64 + 1.0))
        })
        .collect();
    let rows: Vec<Row> = tried
        .into_iter()
        .map(|tried| Row {
            parameter: tried.trial.parameter,
            generic: same_plan(&tried.custom, &tried.generic),
            empty: proves_empty(&tried.custom),
            plan: brief(&tried.custom, &tables),
            sample: tried.trial.sample,
            values: tried.trial.values,
            measured: tried.measured,
            timed_out: tried.timed_out,
        })
        .collect();

    let mut notes = Vec::new();
    if rows.iter().any(|row| row.parameter.is_some()) {
        notes.push("Values come from the column statistics, or are common row counts for a LIMIT or an OFFSET, and are tried one parameter at a time, the others held at a typical value; how columns depend on each other is not taken into account.".to_owned());
    }
    if converted {
        notes.push("The statement's JDBC placeholders (?) were read as $1, $2, …".to_owned());
    }
    let missing: Vec<&Parameter> = parameters
        .iter()
        .filter(|parameter| parameter.held.is_none())
        .collect();

    let other: Vec<&Row> = rows
        .iter()
        .filter(|row| !row.generic && !row.empty)
        .collect();
    let hurt = rows.iter().any(Row::hurt);
    let verdict = if rows.is_empty() {
        Verdict::Unknown
    } else if other.is_empty() {
        Verdict::Insensitive
    } else if !hurt && other.iter().all(|row| row.measured.is_some()) {
        Verdict::Harmless
    } else {
        Verdict::Sensitive
    };

    let switch = generic_brief
        .cost
        .filter(|_| !custom_costs.is_empty())
        .map(|cost| switch(cost, &custom_costs));
    // The value a row tries, or all the values given.
    let tried_value = |row: &Row| match row.parameter.and_then(|number| parameters.get(number - 1))
    {
        Some(parameter) => parameter.show(
            row.sample
                .as_ref()
                .and_then(|sample| sample.value.as_deref()),
        ),
        None => show_all(&parameters, &row.values),
    };
    let summary = match verdict {
        Verdict::Unknown if !missing.is_empty() => format!(
            "No values to try: {}.",
            missing
                .iter()
                .map(|parameter| match &parameter.column {
                    Some(column) => format!(
                        "${} is compared with {}, which has no statistics (ANALYZE gathers them)",
                        parameter.number,
                        column.name()
                    ),
                    None => format!(
                        "${} is not compared with a column in a scan's conditions, nor a LIMIT or an OFFSET",
                        parameter.number
                    ),
                })
                .collect::<Vec<_>>()
                .join("; ")
        ),
        // Every trial failed: the notes say why.
        Verdict::Unknown => "None of the values could be tried; the notes say why.".to_owned(),
        Verdict::Insensitive if rows.len() == 1 && rows[0].parameter.is_none() => format!(
            "The values given get the generic plan ({}).",
            generic_brief.access
        ),
        Verdict::Insensitive => format!(
            "Every value tried gets the generic plan ({}): whichever plan PostgreSQL uses, it is the same.",
            generic_brief.access
        ),
        Verdict::Harmless => {
            let most = other
                .iter()
                .filter(|row| {
                    row.measured
                        .as_ref()
                        .is_some_and(|comparison| comparison.change == Change::Worse)
                })
                .map(|row| worse_by(row))
                .fold(0.0, f64::max);
            format!(
                "Some values get another plan than the generic one ({}), but measured, the generic plan does {} for them.",
                list(other.iter().map(|row| tried_value(row))),
                if most > 1.0 {
                    format!("at most {} worse", format::factor(most))
                } else {
                    "no worse".to_owned()
                }
            )
        }
        Verdict::Sensitive if hurt => {
            let worst = &rows[worst_row(&rows).expect("a row did worse")];
            let values = show_all(&parameters, &worst.values);
            match &worst.timed_out {
                Some(timeout) => format!(
                    "The plan depends on the values: with {values}, the generic plan ran past the {timeout} timeout{}.",
                    worst
                        .measured
                        .as_ref()
                        .and_then(|comparison| comparison.before.execution_time)
                        .map(|time| format!(", while the custom plan took {}", format::duration(time)))
                        .unwrap_or_default()
                ),
                None => format!(
                    "The plan depends on the values: with {values}, the generic plan does worse than the custom plan: {}.",
                    worst
                        .measured
                        .as_ref()
                        .map(Comparison::details)
                        .unwrap_or_default()
                ),
            }
        }
        Verdict::Sensitive => format!(
            "The plan depends on the values: {} of {} values tried get another plan than the generic one ({}), such as {}.",
            other.len(),
            rows.len(),
            generic_brief.access,
            list(other.iter().map(|row| tried_value(row)))
        ),
    };

    let likely = switch.as_ref().map(|switch| switch.likely);
    let mut advice = Vec::new();
    match verdict {
        Verdict::Sensitive if likely != Some(Likely::Custom) => {
            if !hurt {
                advice.push("Measure the plans to know whether the generic plan does worse for those values: --measure.".to_owned());
            }
            advice.push("Have PostgreSQL plan each execution for its values: set plan_cache_mode = force_custom_plan for the application's connections (in a JDBC URL: options=-c%20plan_cache_mode=force_custom_plan) or its role (ALTER ROLE … SET plan_cache_mode = force_custom_plan). Each execution is then planned again.".to_owned());
            advice.push("Or keep the driver from preparing the statement on the server: prepareThreshold=0 with pgJDBC, for the connection or for this statement (PGStatement.setPrepareThreshold(0)); prepare_threshold = None with psycopg 3.".to_owned());
        }
        Verdict::Sensitive => advice.push("Nothing to change while PostgreSQL plans each execution: it estimates the generic plan above every custom plan, so it does not switch to it. Should statistics or data change that, set plan_cache_mode = force_custom_plan for the application's connections.".to_owned()),
        Verdict::Insensitive | Verdict::Harmless => {
            advice.push("Nothing to change: the generic plan suits the values tried.".to_owned());
        }
        Verdict::Unknown if !missing.is_empty() => advice.push(format!(
            "Give {} a value: --bind {}=VALUE.",
            if missing.len() == 1 { "it" } else { "each" },
            missing[0].number
        )),
        Verdict::Unknown => {}
    }

    let shown = if missing.is_empty() {
        format!(
            "The plan shown is the generic plan, estimated with {}.",
            show_all(&parameters, &held_values(&parameters))
        )
    } else {
        "The plan shown is the generic plan, estimated.".to_owned()
    };
    Sensitivity {
        verdict,
        summary,
        parameters,
        generic: generic_brief,
        shown,
        rows,
        switch,
        advice,
        notes,
        converted,
    }
}

impl Sensitivity {
    /// The row the generic plan did worst with, measured.
    pub fn worst(&self) -> Option<usize> {
        worst_row(&self.rows)
    }

    /// Says that the plan shown is the generic plan measured with `values`.
    pub fn show_measured(&mut self, values: &[Option<String>], worst: bool) {
        self.shown = format!(
            "The plan shown is the generic plan, measured with {}{}.",
            show_all(&self.parameters, values),
            if worst {
                ", the values it does worst with"
            } else {
                ""
            }
        );
    }
}

/// Which plan PostgreSQL would likely use after five executions: the
/// generic plan once it estimates it cheaper than the custom plans of the
/// executions so far on average, with planning.
fn switch(generic: f64, custom: &[f64]) -> Switch {
    let low = custom.iter().copied().fold(f64::INFINITY, f64::min);
    let high = custom.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let range = if high - low < 0.5 {
        format::rows(low.round())
    } else {
        format!(
            "from {} to {}",
            format::rows(low.round()),
            format::rows(high.round())
        )
    };
    let cost = format::rows(generic.round());
    if generic < low {
        Switch {
            likely: Likely::Generic,
            reason: format!(
                "PostgreSQL would switch to the generic plan after five executions, whatever their values: it estimates the generic plan at {cost}, below the custom plan of every value tried ({range} with planning)."
            ),
        }
    } else if generic >= high {
        Switch {
            likely: Likely::Custom,
            reason: format!(
                "PostgreSQL would keep planning each execution: it estimates the generic plan at {cost}, above the custom plan of every value tried ({range} with planning)."
            ),
        }
    } else {
        Switch {
            likely: Likely::Either,
            reason: format!(
                "Whether PostgreSQL switches to the generic plan after five executions depends on their values: it estimates the generic plan at {cost}, the custom plans of the values tried {range} with planning, and switches when the first five cost more on average."
            ),
        }
    }
}

/// The row the generic plan did worst with: one that ran past the timeout,
/// or the most pages, or time, over the custom plan's.
fn worst_row(rows: &[Row]) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.hurt())
        .max_by(|(_, a), (_, b)| worse_by(a).total_cmp(&worse_by(b)))
        .map(|(index, _)| index)
}

/// How much worse the generic plan did: its pages, or time, over the
/// custom plan's; without end when it ran past the timeout.
fn worse_by(row: &Row) -> f64 {
    if row.timed_out.is_some() {
        return f64::INFINITY;
    }
    let Some(comparison) = &row.measured else {
        return 0.0;
    };
    let pages = comparison
        .before
        .pages
        .zip(comparison.after.pages)
        .map(|(custom, generic)| generic as f64 / custom.max(1) as f64);
    let time = comparison
        .before
        .execution_time
        .zip(comparison.after.execution_time)
        .map(|(custom, generic)| generic / custom.max(f64::MIN_POSITIVE));
    pages.or(time).unwrap_or(0.0)
}

/// The first three items, and how many more.
fn list(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    let mut text = items.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
    if items.len() > 3 {
        text.push_str(&format!(" and {} more", items.len() - 3));
    }
    text
}

/// The values the parameters are held at.
fn held_values(parameters: &[Parameter]) -> Vec<Option<String>> {
    parameters
        .iter()
        .map(|parameter| parameter.held.clone())
        .collect()
}

/// `$1 = 'pending', $2 = 10`.
fn show_all(parameters: &[Parameter], values: &[Option<String>]) -> String {
    parameters
        .iter()
        .zip(values)
        .map(|(parameter, value)| parameter.show(value.as_deref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A value as SQL shows it: `'pending'`, `NULL`.
pub fn show_value(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("'{}'", value.replace('\'', "''")),
        None => "NULL".to_owned(),
    }
}

/// Types whose values read as numbers.
const NUMERIC_TYPES: [&str; 7] = [
    "smallint",
    "integer",
    "bigint",
    "numeric",
    "real",
    "double precision",
    "oid",
];

/// A value as SQL shows it for its type: `10` for a number, `'pending'`.
pub fn show_typed(value: Option<&str>, type_name: Option<&str>) -> String {
    match (value, type_name) {
        (Some(value), Some(type_name))
            if NUMERIC_TYPES.contains(&type_name)
                && value.parse::<f64>().is_ok_and(f64::is_finite) =>
        {
            value.to_owned()
        }
        _ => show_value(value),
    }
}

/// A plan in brief: how it reads the tables the parameters are compared
/// with, partitions of them included, or its root when it reads none of
/// them.
pub(crate) fn brief(plan: &Plan, tables: &[String]) -> Brief {
    let patterns: Vec<String> = tables
        .iter()
        .map(|table| fingerprint::blank_numbers(table))
        .collect();
    let scans: Vec<&Node> = plan
        .nodes
        .iter()
        .filter(|node| {
            node.node_type.ends_with("Scan")
                && node.node_type != "Bitmap Index Scan"
                && node.relation_name.as_ref().is_some_and(|name| {
                    tables.contains(name) || patterns.contains(&fingerprint::blank_numbers(name))
                })
        })
        .collect();
    // Without a table to look at, every scan of the plan.
    let scans = if scans.is_empty() {
        plan.nodes
            .iter()
            .filter(|node| {
                node.node_type.ends_with("Scan")
                    && node.node_type != "Bitmap Index Scan"
                    && node.relation_name.is_some()
            })
            .collect()
    } else {
        scans
    };
    let access = if scans.is_empty() {
        format::node(plan.root())
    } else {
        // Partitions read the same way are counted, not listed.
        let mut groups: Vec<(String, String, usize)> = Vec::new();
        for scan in scans {
            let label = crate::diff::access_label(plan, scan);
            let pattern = fingerprint::blank_numbers(&label);
            match groups.iter_mut().find(|(_, other, _)| *other == pattern) {
                Some((_, _, count)) => *count += 1,
                None => groups.push((label, pattern, 1)),
            }
        }
        groups
            .into_iter()
            .map(|(label, _, count)| match count {
                1 => label,
                _ => format!("{label} and {} more like it", count - 1),
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Brief {
        shape: fingerprint::id(plan),
        access,
        cost: crate::compare::planner_cost(plan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_placeholders() {
        let jdbc = placeholders(
            "SELECT * FROM orders WHERE status = ? AND note <> '?' AND data ?? 'key' /* ? */ AND customer_id = ? -- ?\n",
        );
        assert_eq!(
            jdbc.sql,
            "SELECT * FROM orders WHERE status = $1 AND note <> '?' AND data ? 'key' /* ? */ AND customer_id = $2 -- ?\n"
        );
        assert_eq!(jdbc.count, 2);
        assert!(jdbc.converted);
        // PostgreSQL's own placeholders stay; a ? is then an operator.
        let dollars = placeholders("SELECT * FROM t WHERE a = $2 AND b = $1 AND j ? 'k'");
        assert_eq!(dollars.count, 2);
        assert!(!dollars.converted);
        assert_eq!(
            dollars.sql,
            "SELECT * FROM t WHERE a = $2 AND b = $1 AND j ? 'k'"
        );
        // Not placeholders: dollar quotes, quoted identifiers, E'' escapes.
        let quoted =
            placeholders("SELECT $f$ ? $f$, \"a?\", E'it\\'s ?', $$?$$ FROM t WHERE x = ?");
        assert_eq!(quoted.count, 1);
        assert!(quoted.sql.ends_with("WHERE x = $1"), "{}", quoted.sql);
        assert_eq!(placeholders("SELECT 1").count, 0);
        // A name that ends in digits is not a parameter.
        assert_eq!(placeholders("SELECT x$1 FROM t").count, 0);
    }

    #[test]
    fn finds_the_parameters_that_count_rows() {
        assert_eq!(
            clauses("SELECT * FROM t WHERE a = $1 ORDER BY b LIMIT $2 OFFSET $3"),
            [(2, Clause::Limit), (3, Clause::Offset)]
        );
        // As Hibernate writes a page.
        assert_eq!(
            clauses(
                "select o.id from orders o order by o.id offset $1 rows fetch first $2 rows only"
            ),
            [(1, Clause::Offset), (2, Clause::Limit)]
        );
        assert_eq!(clauses("SELECT 1 LIMIT (($1))"), [(1, Clause::Limit)]);
        // Not in a literal or a comment, and not a constant.
        assert!(clauses("SELECT 'LIMIT $1' -- LIMIT $2\n LIMIT 10").is_empty());
        assert!(clauses("SELECT limit_$1 FROM t").is_empty());
    }

    fn plan(text: &str) -> Plan {
        crate::parse(text).unwrap()
    }

    #[test]
    fn maps_parameters_to_columns() {
        let generic = plan(
            "\
Limit  (cost=0.29..300.00 rows=10 width=8)
  ->  Nested Loop  (cost=0.29..300.00 rows=10 width=8)
        ->  Bitmap Heap Scan on orders o  (cost=10.00..200.00 rows=100 width=8)
              Recheck Cond: (o.created_at >= $2)
              Filter: (((o.status)::text = $1) AND (o.note = ANY (ARRAY[$5, $6])))
              ->  Bitmap Index Scan on orders_created_at_idx  (cost=0.00..10.00 rows=300 width=0)
                    Index Cond: (o.created_at >= $2)
        ->  Index Scan using customers_pkey on customers c  (cost=0.29..1.00 rows=1 width=4)
              Index Cond: (c.id = o.customer_id)
              Filter: ($3 <= c.created_at)",
        );
        let sql = "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = $1 AND o.created_at >= $2 AND $3 <= c.created_at AND o.note IN ($5, $6) LIMIT $4";
        let parameters = parameters(
            &generic,
            sql,
            &["text".to_owned(), "timestamptz".to_owned()],
        );
        let roles: Vec<Option<String>> = parameters.iter().map(Parameter::role).collect();
        assert_eq!(
            roles,
            [
                // A cast of the column is still the column.
                Some("orders.status =".to_owned()),
                Some("orders.created_at >=".to_owned()),
                // Flipped, so that the column is on the left.
                Some("customers.created_at >=".to_owned()),
                Some("LIMIT".to_owned()),
                // An IN list.
                Some("orders.note =".to_owned()),
                Some("orders.note =".to_owned()),
            ]
        );
        assert_eq!(parameters[1].type_name.as_deref(), Some("timestamptz"));
        assert_eq!(parameters[3].type_name, None);
        assert_eq!(parameter_number("($3)::text"), Some(3));
        assert_eq!(parameter_number("'$3'::text"), None);
    }

    fn status_stats() -> ColumnStats {
        ColumnStats {
            table: "orders".to_owned(),
            null_frac: 0.0,
            n_distinct: 5.0,
            common_values: ["delivered", "shipped", "pending", "cancelled", "refunded"]
                .map(str::to_owned)
                .to_vec(),
            common_freqs: vec![0.70, 0.20, 0.05, 0.04, 0.01],
            histogram: Vec::new(),
        }
    }

    fn dates() -> ColumnStats {
        ColumnStats {
            histogram: (0..101).map(|i| format!("2024-{i:03}")).collect(),
            n_distinct: -0.5,
            ..ColumnStats::default()
        }
    }

    #[test]
    fn picks_values_from_the_statistics() {
        let picked: Vec<(Option<String>, SampleKind)> = samples(&status_stats(), "=")
            .into_iter()
            .map(|sample| (sample.value, sample.kind))
            .collect();
        assert_eq!(
            picked,
            [
                (Some("delivered".to_owned()), SampleKind::Common),
                (Some("shipped".to_owned()), SampleKind::Common),
                (Some("refunded".to_owned()), SampleKind::LeastCommon),
            ]
        );
        assert_eq!(
            samples(&status_stats(), "=")[2].describe(),
            "least common of the most common, 1.0% of rows"
        );
        // A value outside the most common ones, from the histogram.
        let ids = ColumnStats {
            n_distinct: 20_000.0,
            common_values: vec!["15453".to_owned()],
            common_freqs: vec![0.0003],
            histogram: (0..101).map(|i| (i * 200).to_string()).collect(),
            ..ColumnStats::default()
        };
        let picked = samples(&ids, "=");
        assert_eq!(picked.last().unwrap().value.as_deref(), Some("10000"));
        assert_eq!(picked.last().unwrap().kind, SampleKind::Uncommon);
        // Ranges: from the start of the histogram to its end.
        let picked = samples(&dates(), ">=");
        let shares: Vec<String> = picked
            .iter()
            .map(|sample| {
                format!(
                    "{} {:.2}",
                    sample.value.as_deref().unwrap(),
                    sample.share.unwrap()
                )
            })
            .collect();
        assert_eq!(
            shares,
            [
                "2024-000 1.00",
                "2024-025 0.75",
                "2024-050 0.50",
                "2024-075 0.25",
                "2024-100 0.00"
            ]
        );
        assert!(samples(&ColumnStats::default(), "=").is_empty());
    }

    /// `status = $1 AND created_at <= $2 … LIMIT $3`, and `$4`, which the
    /// statement uses otherwise.
    fn four() -> (Vec<Parameter>, Vec<Vec<Sample>>) {
        let column = |name: &str, operator: &str| ColumnUse {
            schema: None,
            table: "orders".to_owned(),
            partitioned: None,
            column: name.to_owned(),
            operator: operator.to_owned(),
        };
        let mut parameters: Vec<Parameter> =
            (1..=4).map(|number| Parameter::new(number, None)).collect();
        parameters[0].column = Some(column("status", "="));
        parameters[1].column = Some(column("created_at", "<="));
        parameters[2].clause = Some(Clause::Limit);
        let samples = vec![
            samples(&status_stats(), "="),
            samples(&dates(), "<="),
            row_counts(Clause::Limit),
            Vec::new(),
        ];
        (parameters, samples)
    }

    #[test]
    fn tries_one_parameter_at_a_time_the_others_held() {
        let (mut parameters, samples) = four();
        hold(&mut parameters, &samples, &[]);
        let held: Vec<Option<&str>> = parameters
            .iter()
            .map(|parameter| parameter.held.as_deref())
            .collect();
        // The most common value, the end of the range that keeps every
        // row, a page; nothing for $4.
        assert_eq!(
            held,
            [Some("delivered"), Some("2024-100"), Some("10"), None]
        );
        // Without a value for every parameter, nothing runs.
        assert!(trials(&parameters, &samples).is_empty());

        hold(
            &mut parameters,
            &samples,
            &[None, None, None, Some("x".to_owned())],
        );
        assert!(parameters[3].given);
        let tried = trials(&parameters, &samples);
        assert_eq!(tried.len(), 3 + 5 + 5);
        let limit_1 = tried
            .iter()
            .find(|trial| trial.parameter == Some(3))
            .unwrap();
        assert_eq!(
            limit_1.values,
            ["delivered", "2024-100", "1", "x"].map(|value| Some(value.to_owned()))
        );

        // Every value given: those alone.
        let given: Vec<Option<String>> = ["pending", "2024-050", "20", "x"]
            .map(|value| Some(value.to_owned()))
            .to_vec();
        hold(&mut parameters, &samples, &given);
        let tried = trials(&parameters, &samples);
        assert_eq!(tried.len(), 1);
        assert_eq!(tried[0].parameter, None);
        assert_eq!(tried[0].values, given);
    }

    const GENERIC: &str = "\
Bitmap Heap Scan on orders  (cost=1176.42..4795.70 rows=66667 width=10)
  Recheck Cond: (created_at >= $1)
  ->  Bitmap Index Scan on orders_created_at_idx  (cost=0.00..1159.75 rows=66667 width=0)
        Index Cond: (created_at >= $1)";

    const EARLY: &str = "Seq Scan on orders  (cost=0.00..4917.00 rows=200000 width=10)\n  Filter: (created_at >= '2024-01-01 00:00:00+00'::timestamp with time zone)";

    const LATE: &str = "Index Scan using orders_created_at_idx on orders  (cost=0.42..8.44 rows=1 width=10)\n  Index Cond: (created_at >= '2025-12-31 00:00:00+00'::timestamp with time zone)";

    fn tried(value: &str, custom: &str) -> Tried {
        Tried {
            trial: Trial {
                parameter: Some(1),
                sample: Some(Sample {
                    value: Some(value.to_owned()),
                    share: None,
                    kind: SampleKind::Percentile(50),
                }),
                values: vec![Some(value.to_owned())],
            },
            custom: plan(custom),
            generic: plan(GENERIC),
            measured: None,
            timed_out: None,
        }
    }

    fn created_at() -> Vec<Parameter> {
        let mut parameters = parameters(
            &plan(GENERIC),
            "SELECT * FROM orders WHERE created_at >= $1",
            &["timestamp with time zone".to_owned()],
        );
        hold(&mut parameters, &[samples(&dates(), ">=")], &[]);
        parameters
    }

    #[test]
    fn tells_whether_the_plan_depends_on_the_values() {
        let sensitive = sensitivity(
            &plan(GENERIC),
            created_at(),
            vec![tried("2024-01-01", EARLY), tried("2025-12-31", LATE)],
            false,
        );
        assert_eq!(sensitive.verdict, Verdict::Sensitive);
        assert_eq!(
            sensitive.summary,
            "The plan depends on the values: 2 of 2 values tried get another plan than the generic one (Bitmap Heap Scan on orders (through orders_created_at_idx)), such as $1 = '2024-01-01'; $1 = '2025-12-31'."
        );
        assert_eq!(
            sensitive.shown,
            "The plan shown is the generic plan, estimated with $1 = '2024-000'."
        );
        // Custom plans cost 13 and 4,922 with planning, and the generic
        // plan 4,796: which one PostgreSQL keeps depends on the values.
        let switch = sensitive.switch.as_ref().unwrap();
        assert_eq!(switch.likely, Likely::Either);
        assert!(
            switch.reason.contains("from 13 to 4,922"),
            "{}",
            switch.reason
        );
        assert!(sensitive.advice[0].contains("--measure"));
        assert!(sensitive.advice[1].starts_with("Have PostgreSQL plan each execution"));

        // Only values that make the custom plan cheaper: PostgreSQL would
        // keep planning each execution.
        let replans = sensitivity(
            &plan(GENERIC),
            created_at(),
            vec![tried("2025-12-31", LATE)],
            false,
        );
        assert_eq!(replans.switch.unwrap().likely, Likely::Custom);
        assert!(replans.advice[0].starts_with("Nothing to change while PostgreSQL plans"));

        // A value that selects no row by its own logic is left out.
        let empty = "Result  (cost=0.00..0.00 rows=0 width=0)\n  One-Time Filter: false";
        let nothing = sensitivity(
            &plan(GENERIC),
            created_at(),
            vec![tried("2025-01-01", GENERIC), tried("2026-01-01", empty)],
            false,
        );
        assert_eq!(nothing.verdict, Verdict::Insensitive);
        assert!(nothing.rows[1].empty);

        // The same plan for every value.
        let same = sensitivity(
            &plan(GENERIC),
            created_at(),
            vec![tried("2025-01-01", GENERIC)],
            true,
        );
        assert_eq!(same.verdict, Verdict::Insensitive);
        assert!(
            same.notes
                .iter()
                .any(|note| note.contains("JDBC placeholders"))
        );
        assert_eq!(
            sensitivity(&plan(GENERIC), created_at(), Vec::new(), false).verdict,
            Verdict::Unknown
        );
    }

    #[test]
    fn says_which_parameters_have_no_value() {
        let mut parameters = created_at();
        hold(&mut parameters, &[Vec::new()], &[]);
        let unknown = sensitivity(&plan(GENERIC), parameters, Vec::new(), false);
        assert_eq!(unknown.verdict, Verdict::Unknown);
        assert_eq!(
            unknown.summary,
            "No values to try: $1 is compared with orders.created_at, which has no statistics (ANALYZE gathers them)."
        );
        assert_eq!(unknown.advice, ["Give it a value: --bind 1=VALUE."]);
        assert_eq!(
            unknown.shown,
            "The plan shown is the generic plan, estimated."
        );
    }

    #[test]
    fn partitions_pruned_as_the_generic_plan_starts_are_the_same_plan() {
        let pruned = "\
Append  (cost=4.81..1183.72 rows=599 width=63)
  Subplans Removed: 11
  ->  Bitmap Heap Scan on events_2025_03 events_1  (cost=4.81..100.67 rows=51 width=63)
        Recheck Cond: (created_at >= $1)
        ->  Bitmap Index Scan on events_2025_03_created_at_idx  (cost=0.00..4.79 rows=51 width=0)
              Index Cond: (created_at >= $1)";
        let custom = "\
Bitmap Heap Scan on events_2025_03 events  (cost=11.80..137.95 rows=343 width=63)
  Recheck Cond: (created_at >= '2025-03-01 00:00:00+00'::timestamp with time zone)
  ->  Bitmap Index Scan on events_2025_03_created_at_idx  (cost=0.00..11.71 rows=343 width=0)
        Index Cond: (created_at >= '2025-03-01 00:00:00+00'::timestamp with time zone)";
        let mut trial = tried("2025-03-01", custom);
        trial.generic = plan(pruned);
        let mut parameters = parameters(
            &plan(pruned),
            "SELECT * FROM events WHERE created_at >= $1",
            &["timestamp with time zone".to_owned()],
        );
        hold(&mut parameters, &[samples(&dates(), ">=")], &[]);
        let result = sensitivity(&plan(pruned), parameters, vec![trial], false);
        assert_eq!(result.verdict, Verdict::Insensitive);
        assert_eq!(
            result.generic.access,
            "Bitmap Heap Scan on events_2025_03 events_1 (through events_2025_03_created_at_idx)"
        );
    }

    #[test]
    fn measured_values_say_what_the_generic_plan_costs() {
        let custom = "Index Scan using orders_created_at_idx on orders  (cost=0.42..8.44 rows=1 width=10) (actual time=0.010..0.020 rows=1 loops=1)\n  Index Cond: (created_at >= '2025-12-31'::timestamp with time zone)\n  Buffers: shared hit=4\nExecution Time: 0.030 ms";
        let generic_run = |pages: u32| {
            format!(
                "Bitmap Heap Scan on orders  (cost=1176.42..4795.70 rows=66667 width=10) (actual time=0.010..0.020 rows=1 loops=1)\n  Recheck Cond: (created_at >= $1)\n  Buffers: shared hit={pages}\n  ->  Bitmap Index Scan on orders_created_at_idx  (cost=0.00..1159.75 rows=66667 width=0) (actual time=0.005..0.005 rows=1 loops=1)\n        Index Cond: (created_at >= $1)\n        Buffers: shared hit=3\nExecution Time: 0.040 ms"
            )
        };
        let mut worse = tried("2025-12-31", custom);
        worse.measured = Some(crate::compare::compare(
            &plan(custom),
            &plan(&generic_run(40)),
        ));
        let mut result = sensitivity(&plan(GENERIC), created_at(), vec![worse], false);
        assert_eq!(result.verdict, Verdict::Sensitive);
        assert!(
            result.summary.starts_with(
                "The plan depends on the values: with $1 = '2025-12-31', the generic plan does worse than the custom plan: pages 4 → 40 (10× more)"
            ),
            "{}",
            result.summary
        );
        // PostgreSQL estimates this custom plan far below the generic plan,
        // so it would not switch to the generic plan.
        assert_eq!(result.switch.as_ref().unwrap().likely, Likely::Custom);
        assert!(result.advice[0].starts_with("Nothing to change while PostgreSQL plans"));
        assert_eq!(result.worst(), Some(0));
        // A custom plan estimated above the generic plan: PostgreSQL would
        // switch to the generic plan, which does worse.
        let dear = "Seq Scan on orders  (cost=0.00..4917.00 rows=1 width=10) (actual time=0.010..0.020 rows=1 loops=1)\n  Filter: (created_at >= '2025-12-31'::timestamp with time zone)\n  Buffers: shared hit=4\nExecution Time: 0.030 ms";
        let mut switched = tried("2025-12-31", dear);
        switched.measured = Some(crate::compare::compare(
            &plan(dear),
            &plan(&generic_run(40)),
        ));
        let switched = sensitivity(&plan(GENERIC), created_at(), vec![switched], false);
        assert_eq!(switched.switch.as_ref().unwrap().likely, Likely::Generic);
        assert!(switched.advice[0].starts_with("Have PostgreSQL plan each execution"));
        assert!(switched.advice[1].contains("prepareThreshold=0"));
        result.show_measured(&[Some("2025-12-31".to_owned())], true);
        assert_eq!(
            result.shown,
            "The plan shown is the generic plan, measured with $1 = '2025-12-31', the values it does worst with."
        );

        // As many pages: another plan, but no worse.
        let mut same = tried("2025-12-31", custom);
        same.measured = Some(crate::compare::compare(&plan(custom), &plan(custom)));
        let harmless = sensitivity(&plan(GENERIC), created_at(), vec![same], false);
        assert_eq!(harmless.verdict, Verdict::Harmless);
        assert!(
            harmless
                .summary
                .ends_with("the generic plan does no worse for them.")
        );
        assert_eq!(harmless.worst(), None);
        // Worse, but less than twice: not worth planning every execution.
        let mut slightly = tried("2025-12-31", custom);
        slightly.measured = Some(crate::compare::compare(
            &plan(custom),
            &plan(&generic_run(6)),
        ));
        let harmless = sensitivity(&plan(GENERIC), created_at(), vec![slightly], false);
        assert_eq!(harmless.verdict, Verdict::Harmless);
        assert!(
            harmless
                .summary
                .ends_with("the generic plan does at most 1.5× worse for them."),
            "{}",
            harmless.summary
        );
        assert!(harmless.advice[0].starts_with("Nothing to change"));

        // The generic plan ran past the timeout.
        let mut stopped = tried("2025-12-31", custom);
        stopped.measured = Some(crate::compare::compare_runs(&[plan(custom)], &[]));
        stopped.timed_out = Some("30 s".to_owned());
        let timed_out = sensitivity(&plan(GENERIC), created_at(), vec![stopped], false);
        assert_eq!(timed_out.verdict, Verdict::Sensitive);
        assert_eq!(
            timed_out.summary,
            "The plan depends on the values: with $1 = '2025-12-31', the generic plan ran past the 30 s timeout, while the custom plan took 0.030 ms."
        );
    }

    #[test]
    fn shows_values_as_sql_does() {
        assert_eq!(show_value(Some("it's")), "'it''s'");
        assert_eq!(show_value(None), "NULL");
        assert_eq!(show_typed(Some("4242"), Some("integer")), "4242");
        assert_eq!(show_typed(Some("NaN"), Some("numeric")), "'NaN'");
        assert_eq!(show_typed(Some("4242"), Some("text")), "'4242'");
        assert_eq!(
            Parameter {
                type_name: Some("bigint".to_owned()),
                ..Parameter::new(2, None)
            }
            .show(Some("10")),
            "$2 = 10"
        );
    }
}
