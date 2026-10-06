//! Anonymizing plans so that they can be shared: in a bug report, an issue
//! or a chat with someone outside the team.
//!
//! The plan keeps its shape, its numbers and its node types, so that it
//! reads and analyzes as before. What could tell about the data or the
//! schema is replaced, the same way everywhere it appears:
//!
//! - the names of tables, indexes, CTEs, aliases, schemas, columns,
//!   constraints and triggers (`table_a`, `index_a`, `column_a`, …);
//! - literal values, strings as `'value_a'` (keeping the `%` of a `LIKE`
//!   pattern at either end) and numbers as small ones.
//!
//! Names that differ only in their numbers, such as the partitions
//! `orders_2025_01` and `orders_2025_02`, become names that differ only in
//! their numbers (`table_a_1`, `table_a_2`), and other names become names
//! without any: explainsql groups and matches partitions by their names
//! with the numbers left out, in the viewer, in `diff` and in plan shapes,
//! and the anonymized plan must group and match the same way.
//!
//! Kept: function and type names, keywords, `$n` parameters, system names
//! (`pg_catalog`, `public`, `pg_…` relations, `RI_ConstraintTrigger_…`,
//! system columns such as `ctid`), and every measured or estimated number
//! outside of conditions. Wrappers such as psql's table output, server log
//! lines and Markdown fences are removed: the output is the plans
//! themselves, in the format they were written in.
//!
//! ```
//! use explainsql_core::anonymize::{Options, anonymize};
//!
//! let plan = "Seq Scan on orders o  (cost=0.00..4917.00 rows=10 width=64)\n  Filter: (o.customer_id = 4242)";
//! let anonymized = anonymize(plan, Options::default()).unwrap();
//! assert_eq!(
//!     anonymized.text,
//!     "Seq Scan on table_a alias_a  (cost=0.00..4917.00 rows=10 width=64)\n  Filter: (alias_a.column_a = 1)\n",
//! );
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::Serialize;

use crate::fingerprint::blank_numbers;
use crate::ir::{Format, Plan};
use crate::params;
use crate::pg::{self, ParseError};

/// What to keep.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Keep the names of tables, columns and other objects; replace only
    /// literal values.
    pub keep_names: bool,
}

/// The anonymized plans, and what each name and value became.
#[derive(Debug, Clone)]
pub struct Anonymized {
    /// The plans, one after the other, each in its own format.
    pub text: String,
    pub mapping: Mapping,
}

/// What each name and value became, by kind: original → replacement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Mapping {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ctes: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub schemas: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub constraints: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub triggers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
}

/// Why plans could not be anonymized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input holds no plan.
    Parse(ParseError),
    /// The anonymized plans could not be read back as the same plans. This
    /// is a bug; nothing is printed, so that nothing half-anonymized leaks.
    Unreadable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(error) => error.fmt(f),
            Error::Unreadable => f.write_str(
                "the anonymized plan could not be read back; please report this with the plan's structure",
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Anonymizes every plan of the input (see the [module](self) for what
/// changes). The same name or value gets the same replacement in every plan
/// of the input.
pub fn anonymize(input: &str, options: Options) -> Result<Anonymized, Error> {
    let plans = pg::parse_all(input).map_err(Error::Parse)?;
    let mut namer = Namer::new(options);
    // Names are given in the order of the plans' nodes, whatever the
    // format, so that the JSON and the text of a plan anonymize alike.
    for plan in &plans {
        namer.learn(plan);
    }
    let mut parts = Vec::new();
    for part in pg::normalize_all(input) {
        // Only parts that hold a plan: anything else could be anything.
        if part.text.trim().is_empty() || pg::parse_all(&part.text).is_err() {
            continue;
        }
        let text = match part.format {
            Format::Json => namer.json(&part.text),
            Format::Text => namer.text_plan(&part.text),
        };
        parts.push((part.format, text.trim_end().to_owned()));
    }
    // JSON plans follow each other as they are, and so do text plans; a
    // mix is told apart by Markdown fences.
    let mixed = parts.windows(2).any(|pair| pair[0].0 != pair[1].0);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|(_, text)| {
            if mixed {
                format!("```\n{text}\n```")
            } else {
                text
            }
        })
        .collect();
    let mut text = parts.join("\n\n");
    text.push('\n');
    match pg::parse_all(&text) {
        Ok(again) if same_nodes(&plans, &again) => Ok(Anonymized {
            text,
            mapping: namer.mapping,
        }),
        _ => Err(Error::Unreadable),
    }
}

/// Whether two lists of plans have the same nodes, of the same types.
fn same_nodes(a: &[Plan], b: &[Plan]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.nodes.len() == b.nodes.len()
                && a.nodes
                    .iter()
                    .zip(&b.nodes)
                    .all(|(x, y)| x.node_type == y.node_type)
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Table,
    Index,
    Cte,
    Alias,
    Schema,
    Column,
    Constraint,
    Trigger,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::Index => "index",
            Kind::Cte => "cte",
            Kind::Alias => "alias",
            Kind::Schema => "schema",
            Kind::Column => "column",
            Kind::Constraint => "constraint",
            Kind::Trigger => "trigger",
        }
    }

    /// Tables, indexes, CTEs and aliases share one namespace: an alias is
    /// the relation's name when the statement gave none.
    fn namespace(self) -> u8 {
        match self {
            Kind::Table | Kind::Index | Kind::Cte | Kind::Alias => 0,
            Kind::Schema => 1,
            Kind::Column => 2,
            Kind::Constraint => 3,
            Kind::Trigger => 4,
        }
    }
}

/// Hands out replacements, the same one for the same name every time.
struct Namer {
    options: Options,
    names: HashMap<(u8, String), String>,
    labels: Labels,
    strings: HashMap<String, String>,
    numbers: HashMap<String, String>,
    mapping: Mapping,
}

impl Namer {
    fn new(options: Options) -> Self {
        Namer {
            options,
            names: HashMap::new(),
            labels: Labels::default(),
            strings: HashMap::new(),
            numbers: HashMap::new(),
            mapping: Mapping::default(),
        }
    }

    /// Gives every name of the plan its replacement, in the order of the
    /// nodes.
    fn learn(&mut self, plan: &Plan) {
        for (_, node) in plan.walk() {
            if let Some(schema) = &node.schema {
                self.name(Kind::Schema, schema);
            }
            if let Some(relation) = &node.relation_name {
                self.name(Kind::Table, relation);
            }
            if let Some(cte) = &node.cte_name {
                self.name(Kind::Cte, cte);
            }
            if let Some(index) = &node.index_name {
                self.name(Kind::Index, index);
            }
            if let Some(alias) = &node.alias {
                self.name(Kind::Alias, alias);
            }
        }
        for (_, node) in plan.walk() {
            let lists = [
                &node.output,
                &node.sort_key,
                &node.presorted_key,
                &node.group_key,
            ];
            for item in lists.into_iter().flatten() {
                self.expression(item, false);
            }
            for predicate in &node.predicates {
                self.expression(&predicate.text, false);
            }
        }
    }

    /// The replacement for a name, or the name itself when it is kept.
    fn name(&mut self, kind: Kind, original: &str) -> String {
        if self.keeps(kind, original) {
            return original.to_owned();
        }
        let key = (kind.namespace(), original.to_owned());
        if let Some(name) = self.names.get(&key) {
            return name.clone();
        }
        let name = self.labels.label(kind.namespace(), kind.prefix(), original);
        let map = match kind {
            Kind::Table => &mut self.mapping.tables,
            Kind::Index => &mut self.mapping.indexes,
            Kind::Cte => &mut self.mapping.ctes,
            Kind::Alias => &mut self.mapping.aliases,
            Kind::Schema => &mut self.mapping.schemas,
            Kind::Column => &mut self.mapping.columns,
            Kind::Constraint => &mut self.mapping.constraints,
            Kind::Trigger => &mut self.mapping.triggers,
        };
        map.insert(original.to_owned(), name.clone());
        self.names.insert(key, name.clone());
        name
    }

    /// The replacement of a name known to be a relation, alias or CTE.
    fn known_relation(&self, original: &str) -> Option<String> {
        self.names.get(&(0, original.to_owned())).cloned()
    }

    /// Whether the plans name a relation, an alias, a CTE or a column so.
    fn is_known(&self, original: &str) -> bool {
        [Kind::Table, Kind::Column].iter().any(|kind| {
            self.names
                .contains_key(&(kind.namespace(), original.to_owned()))
        })
    }

    fn keeps(&self, kind: Kind, name: &str) -> bool {
        if self.options.keep_names || name.is_empty() || name.starts_with('*') {
            return true;
        }
        match kind {
            Kind::Schema => {
                matches!(name, "public" | "pg_catalog" | "information_schema")
                    || name.starts_with("pg_")
            }
            Kind::Table | Kind::Index => name.starts_with("pg_"),
            Kind::Column => SYSTEM_COLUMNS.contains(&name),
            Kind::Trigger => name.starts_with("RI_ConstraintTrigger"),
            Kind::Cte | Kind::Alias | Kind::Constraint => false,
        }
    }

    /// `'value_a'`, keeping the `%` of a `LIKE` pattern at either end.
    fn string(&mut self, content: &str) -> String {
        if let Some(string) = self.strings.get(content) {
            return string.clone();
        }
        let name = self.labels.label(STRINGS, "value", content);
        let start = if content.starts_with('%') { "%" } else { "" };
        let end = if content.len() > 1 && content.ends_with('%') {
            "%"
        } else {
            ""
        };
        let string = format!("'{start}{name}{end}'");
        self.mapping
            .values
            .insert(format!("'{content}'"), string.clone());
        self.strings.insert(content.to_owned(), string.clone());
        string
    }

    /// Another number of the same form: its first run of digits counts the
    /// numbers met so far, its other runs are zeros (`1.99` → `3.0`).
    fn number(&mut self, text: &str) -> String {
        if let Some(number) = self.numbers.get(text) {
            return number.clone();
        }
        let count = (self.numbers.len() + 1).to_string();
        let mut number = String::with_capacity(text.len());
        let mut runs = 0;
        let mut in_run = false;
        for c in text.chars() {
            if c.is_ascii_digit() {
                if !in_run {
                    number.push_str(if runs == 0 { &count } else { "0" });
                    runs += 1;
                }
                in_run = true;
            } else {
                number.push(c);
                in_run = false;
            }
        }
        self.mapping.values.insert(text.to_owned(), number.clone());
        self.numbers.insert(text.to_owned(), number.clone());
        number
    }

    /// A name as written in a plan or a statement, quoted or not.
    fn written(&mut self, kind: Kind, identifier: &Identifier) -> String {
        if self.keeps(kind, &identifier.name) {
            identifier.raw.clone()
        } else {
            self.name(kind, &identifier.name)
        }
    }

    /// Rewrites a condition, an output column, a sort key or (with `sql`)
    /// a whole statement.
    fn expression(&mut self, text: &str, sql: bool) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        // The last keyword, for what follows it (COLLATE "C").
        let mut keyword = "";
        while let Some(c) = rest.chars().next() {
            let previous_word = out
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
            // E'…' takes backslash escapes.
            let escapes = matches!(c, 'E' | 'e') && rest[1..].starts_with('\'') && !previous_word;
            if c == '\'' || escapes {
                let literal = if escapes { &rest[1..] } else { rest };
                let length = params::quoted(literal, '\'', escapes);
                let content = literal[1..length]
                    .strip_suffix('\'')
                    .unwrap_or(&literal[1..length]);
                out.push_str(&self.string(content));
                rest = &literal[length..];
            } else if sql && rest.starts_with("--") {
                rest = &rest[rest.find('\n').unwrap_or(rest.len())..];
                out.push(' ');
            } else if sql && rest.starts_with("/*") {
                rest = &rest[params::block_comment(rest)..];
                out.push(' ');
            } else if c == '$' {
                if let Some(length) = params::dollar_quote(rest) {
                    let tag = rest[1..].find('$').map_or(1, |end| end + 2);
                    let content = &rest[tag..length.saturating_sub(tag).max(tag)];
                    out.push_str(&self.string(content));
                    rest = &rest[length..];
                } else {
                    // A parameter, $1: kept.
                    let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
                    out.push_str(&rest[..1 + digits]);
                    rest = &rest[1 + digits..];
                }
            } else if rest.starts_with("::") {
                out.push_str("::");
                rest = cast_type(&rest[2..], &mut out);
            } else if c.is_ascii_digit() && !previous_word {
                let length = number_length(rest);
                if out.ends_with("InitPlan ") || out.ends_with("SubPlan ") {
                    out.push_str(&rest[..length]);
                } else {
                    out.push_str(&self.number(&rest[..length]));
                }
                rest = &rest[length..];
            } else if c == '"' || is_identifier_start(c) {
                let (chain, after) = identifier_chain(rest);
                if chain.is_empty() {
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                    continue;
                }
                let written = self.chain(&chain, after, &out, keyword, sql);
                keyword = match chain.as_slice() {
                    [word] if !word.quoted && is_keyword(&word.raw, sql) => KEYWORDS
                        .iter()
                        .chain(SQL_KEYWORDS)
                        .find(|k| k.eq_ignore_ascii_case(&word.raw))
                        .copied()
                        .unwrap_or(""),
                    _ => "",
                };
                out.push_str(&written);
                rest = after;
            } else {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        out
    }

    /// A name or a dotted chain of names in an expression.
    fn chain(
        &mut self,
        chain: &[Identifier],
        after: &str,
        before: &str,
        keyword: &str,
        sql: bool,
    ) -> String {
        // A statement's names fold to lower case unless quoted, as in
        // PostgreSQL: `FROM Orders` reads `orders`.
        let folded: Vec<Identifier>;
        let chain = if sql {
            folded = chain.iter().map(Identifier::folded).collect();
            folded.as_slice()
        } else {
            chain
        };
        let raw = || {
            chain
                .iter()
                .map(|part| part.raw.as_str())
                .collect::<Vec<_>>()
                .join(".")
        };
        // Functions keep their names, not their schemas: count(*),
        // lower(email), app.tax(amount).
        if let Some((function, schemas)) = chain.split_last() {
            if after.starts_with('(') && !function.quoted {
                let mut out = String::new();
                for schema in schemas {
                    out.push_str(&self.written(Kind::Schema, schema));
                    out.push('.');
                }
                out.push_str(&function.raw);
                return out;
            }
        }
        if keyword.eq_ignore_ascii_case("COLLATE") {
            return raw();
        }
        match chain {
            [word] => {
                // A statement may name a column `time` or `first`: a keyword
                // that is not reserved is a name where the plans use it so.
                let named = sql && !is_reserved(&word.raw) && self.is_known(&word.name);
                if !word.quoted
                    && !named
                    && (is_keyword(&word.raw, sql) || is_plan_word(&word.raw, after))
                {
                    return word.raw.clone();
                }
                // (InitPlan 1).col1, a window w1.
                if (before.ends_with(").") && is_numbered(&word.raw, "col"))
                    || (is_numbered(&word.raw, "w")
                        && (after.starts_with(" AS (") || before.ends_with("OVER ")))
                {
                    return word.raw.clone();
                }
                if sql {
                    if let Some(name) = self.known_relation(&word.name) {
                        return name;
                    }
                }
                self.written(Kind::Column, word)
            }
            [qualifier, column] => {
                let qualifier = if self.names.contains_key(&(1, qualifier.name.clone()))
                    || self.keeps(Kind::Schema, &qualifier.name)
                {
                    self.written(Kind::Schema, qualifier)
                } else if qualifier.name == "excluded" {
                    qualifier.raw.clone()
                } else {
                    self.written(Kind::Alias, qualifier)
                };
                let column = match self.known_relation(&column.name) {
                    Some(name) if sql => name,
                    _ => self.written(Kind::Column, column),
                };
                format!("{qualifier}.{column}")
            }
            [schema, relation, rest @ ..] => {
                let mut parts = vec![
                    self.written(Kind::Schema, schema),
                    self.written(Kind::Table, relation),
                ];
                for part in rest {
                    parts.push(self.written(Kind::Column, part));
                }
                parts.join(".")
            }
            [] => String::new(),
        }
    }

    /// Rewrites a JSON plan in place: only string values change, so the
    /// document keeps its layout and its key order.
    fn json(&mut self, text: &str) -> String {
        /// An open object or array: the key whose value is being read (an
        /// array repeats the key that holds it), and the key that holds the
        /// object.
        struct Open {
            object: bool,
            key: String,
            owner: String,
        }
        let mut out = String::with_capacity(text.len());
        let mut stack: Vec<Open> = Vec::new();
        let mut expecting_key = false;
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            match c {
                '{' | '[' => {
                    let (key, owner) = stack
                        .last()
                        .map(|open| (open.key.clone(), open.owner.clone()))
                        .unwrap_or_default();
                    stack.push(if c == '{' {
                        Open {
                            object: true,
                            key: String::new(),
                            owner: key,
                        }
                    } else {
                        Open {
                            object: false,
                            key,
                            owner,
                        }
                    });
                    expecting_key = c == '{';
                }
                '}' | ']' => {
                    stack.pop();
                    expecting_key = false;
                }
                ',' => expecting_key = stack.last().is_some_and(|open| open.object),
                ':' => expecting_key = false,
                '"' => {
                    let length = json_string_length(rest);
                    let token = &rest[..length];
                    rest = &rest[length..];
                    let value: Option<String> = serde_json::from_str(token).ok();
                    match stack.last_mut() {
                        Some(open) if open.object && expecting_key => {
                            open.key = value.unwrap_or_default();
                            out.push_str(token);
                        }
                        open => {
                            let (key, owner) = open
                                .map(|open| (open.key.clone(), open.owner.clone()))
                                .unwrap_or_default();
                            let new = match value {
                                Some(value) => self.json_value(&owner, &key, &value),
                                // Unreadable, so nothing of it is kept.
                                None => Some(String::new()),
                            };
                            match new {
                                Some(new) => out.push_str(
                                    &serde_json::to_string(&new).expect("a string serializes"),
                                ),
                                None => out.push_str(token),
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// The replacement for a string value of a JSON plan, by its key and the
    /// key of the object that holds it; `None` keeps it.
    fn json_value(&mut self, owner: &str, key: &str, value: &str) -> Option<String> {
        if owner == "Settings" {
            // Planner settings name nothing, but for the search path.
            return if key == "search_path" {
                self.property(key, value, false)
            } else {
                None
            };
        }
        self.property(key, value, false)
    }

    /// The replacement for the value of a property, by its key (JSON) or
    /// label (text); `None` keeps it.
    fn property(&mut self, key: &str, value: &str, text: bool) -> Option<String> {
        // Text plans repeat a label for grouping sets: `Group Key (2)`.
        let key = key.split(" (").next().unwrap_or(key);
        let names = |namer: &mut Namer, kind: Kind| {
            if text {
                // Names as written: quoted when they need to be.
                let mut out = Vec::new();
                for item in value.split(", ") {
                    let (chain, _) = identifier_chain(item);
                    match chain.as_slice() {
                        [one] => out.push(namer.written(kind, one)),
                        _ => out.push(namer.expression(item, false)),
                    }
                }
                out.join(", ")
            } else {
                namer.name(kind, value)
            }
        };
        let new = match key {
            "Relation Name" | "Relation" => names(self, Kind::Table),
            "Schema" => names(self, Kind::Schema),
            "Alias" => names(self, Kind::Alias),
            "Index Name" | "Conflict Arbiter Indexes" => names(self, Kind::Index),
            "Tuplestore Name" => names(self, Kind::Table),
            "CTE Name" => names(self, Kind::Cte),
            "Constraint Name" => names(self, Kind::Constraint),
            "Trigger Name" => names(self, Kind::Trigger),
            "Subplan Name" => format!("CTE {}", self.name(Kind::Cte, value.strip_prefix("CTE ")?)),
            "search_path" => {
                let mut out = Vec::new();
                for item in value.split(',') {
                    let item = item.trim();
                    let (chain, _) = identifier_chain(item);
                    match chain.as_slice() {
                        [one] if one.name != "$user" => out.push(self.written(Kind::Schema, one)),
                        _ => out.push(item.to_owned()),
                    }
                }
                out.join(", ")
            }
            "Query Text" | "Remote SQL" => self.expression(value, true),
            // `bernoulli ('10'::real) REPEATABLE ('42'::double precision)`:
            // the method is a function.
            "Sampling" => match value.split_once(' ') {
                Some((method, rest)) => format!("{method} {}", self.expression(rest, false)),
                None => value.to_owned(),
            },
            _ if EXPRESSIONS.contains(&key) => self.expression(value, false),
            _ if KEPT.contains(&key) => return None,
            // An unfamiliar property: its value could hold anything.
            _ => self.expression(value, true),
        };
        Some(new)
    }

    /// Rewrites a text plan line by line.
    fn text_plan(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let (line, newline) = match line.strip_suffix('\n') {
                Some(line) => (line, "\n"),
                None => (line, ""),
            };
            let content = line.trim_start();
            out.push_str(&line[..line.len() - content.len()]);
            out.push_str(&self.text_line(content));
            out.push_str(newline);
        }
        out
    }

    fn text_line(&mut self, content: &str) -> String {
        if content.is_empty() {
            return String::new();
        }
        if let Some(header) = content.strip_prefix("->") {
            let name = header.trim_start();
            return format!(
                "->{}{}",
                &header[..header.len() - name.len()],
                self.node_header(name)
            );
        }
        if is_node_line(content) || is_node_name(content) {
            return self.node_header(content);
        }
        if let Some(rest) = content.strip_prefix("Trigger ") {
            if let Some(trigger) = self.trigger(rest) {
                return trigger;
            }
        }
        if let Some((worker, rest)) = content.split_once(':') {
            let number = worker.strip_prefix("Worker ").unwrap_or_default();
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
                // `Worker 0:  actual time=…`, or one space before PostgreSQL 13.
                let property = rest.trim_start();
                let space = &rest[..rest.len() - property.len()];
                let property = self.text_line(property);
                return format!("{worker}:{space}{property}");
            }
        }
        if let Some((label, value)) = content.split_once(": ") {
            if label == "Settings" {
                return format!("Settings: {}", self.settings(value));
            }
            // A label in PostgreSQL's words; anything else may name things.
            if is_label(label) {
                return match self.property(label, value, true) {
                    Some(value) => format!("{label}: {value}"),
                    None => content.to_owned(),
                };
            }
        }
        // `Planning:`, `JIT:`, and a worker's `actual time=… rows=… loops=…`.
        if content
            .strip_suffix(':')
            .is_some_and(|label| KEPT.contains(&label))
            || is_actual(content)
        {
            return content.to_owned();
        }
        if let Some(cte) = content.strip_prefix("CTE ") {
            let (chain, after) = identifier_chain(cte);
            if let [name] = chain.as_slice() {
                return format!("CTE {}{after}", self.written(Kind::Cte, name));
            }
        }
        // Not a line of a plan as explainsql knows it: replace any name.
        self.expression(content, true)
    }

    /// `Index Scan using orders_pkey on public.orders o  (cost=…)`.
    fn node_header(&mut self, content: &str) -> String {
        let split = [" (cost=", " (actual ", " (never executed)"]
            .iter()
            .filter_map(|marker| content.find(marker))
            .min()
            .unwrap_or(content.len());
        let (name, numbers) = content.split_at(split);
        // The node type is PostgreSQL's words; only what follows names
        // anything.
        let (node_type, mut rest) = match name.find(" using ").or_else(|| name.find(" on ")) {
            Some(at) => name.split_at(at),
            None => (name, ""),
        };
        let mut out = node_type.to_owned();
        if let Some(index_text) = rest.strip_prefix(" using ") {
            out.push_str(" using ");
            let (chain, after) = identifier_chain(index_text);
            match chain.split_last() {
                Some((index, schemas)) => {
                    for schema in schemas {
                        out.push_str(&self.written(Kind::Schema, schema));
                        out.push('.');
                    }
                    out.push_str(&self.written(Kind::Index, index));
                    rest = after;
                }
                None => {
                    out.push_str(&self.expression(index_text, true));
                    rest = "";
                }
            }
        }
        if let Some(target) = rest.strip_prefix(" on ") {
            out.push_str(" on ");
            out.push_str(&self.target(node_type, target));
        } else if !rest.is_empty() {
            out.push_str(&self.expression(rest, true));
        }
        out.push_str(numbers);
        out
    }

    /// What a node reads: `[schema.]relation [alias]`.
    fn target(&mut self, node_type: &str, text: &str) -> String {
        let (chain, after) = identifier_chain(text);
        let mut out = match chain.as_slice() {
            [function] if node_type.ends_with("Function Scan") => function.raw.clone(),
            [name] if node_type.ends_with("Bitmap Index Scan") => self.written(Kind::Index, name),
            [name] if node_type.ends_with("CTE Scan") || node_type.ends_with("WorkTable Scan") => {
                self.written(Kind::Cte, name)
            }
            [name] if node_type.ends_with("Subquery Scan") => self.written(Kind::Alias, name),
            [name] => self.written(Kind::Table, name),
            [schema, name] if node_type.ends_with("Function Scan") => {
                format!("{}.{}", self.written(Kind::Schema, schema), name.raw)
            }
            [schema, name] => format!(
                "{}.{}",
                self.written(Kind::Schema, schema),
                self.written(Kind::Table, name)
            ),
            _ => return self.expression(text, true),
        };
        let mut rest = after;
        if let Some(alias_text) = rest.strip_prefix(' ') {
            let (alias, after) = identifier_chain(alias_text);
            if let [alias] = alias.as_slice() {
                out.push(' ');
                out.push_str(&self.written(Kind::Alias, alias));
                rest = after;
            }
        }
        if !rest.trim().is_empty() {
            out.push_str(&self.expression(rest, true));
        } else {
            out.push_str(rest);
        }
        out
    }

    /// `RI_ConstraintTrigger_a_16417 for constraint fk on t: time=1.0 calls=2`.
    fn trigger(&mut self, rest: &str) -> Option<String> {
        let (description, stats) = rest.rsplit_once(": ")?;
        let (description, relation) = match description.rsplit_once(" on ") {
            Some((description, relation)) => (description, Some(relation)),
            None => (description, None),
        };
        let (name, constraint) = match description.split_once("for constraint ") {
            Some((name, constraint)) => (name.trim(), Some(constraint.trim())),
            None => (description.trim(), None),
        };
        let mut out = String::from("Trigger");
        let one = |namer: &mut Namer, kind: Kind, text: &str| -> Option<String> {
            match identifier_chain(text) {
                (chain, "") if chain.len() == 1 => Some(namer.written(kind, &chain[0])),
                (chain, "") if chain.len() == 2 && kind == Kind::Table => Some(format!(
                    "{}.{}",
                    namer.written(Kind::Schema, &chain[0]),
                    namer.written(Kind::Table, &chain[1])
                )),
                _ => None,
            }
        };
        if !name.is_empty() {
            out.push(' ');
            out.push_str(&one(self, Kind::Trigger, name)?);
        }
        if let Some(constraint) = constraint {
            out.push_str(" for constraint ");
            out.push_str(&one(self, Kind::Constraint, constraint)?);
        }
        if let Some(relation) = relation {
            out.push_str(" on ");
            out.push_str(&one(self, Kind::Table, relation.trim())?);
        }
        out.push_str(": ");
        out.push_str(stats);
        Some(out)
    }

    /// `enable_seqscan = 'off', search_path = 'app, public'`: only the
    /// search path names anything.
    fn settings(&mut self, value: &str) -> String {
        split_settings(value)
            .into_iter()
            .map(|setting| match setting.split_once(" = ") {
                Some(("search_path", quoted)) => {
                    let inner = quoted.trim_matches('\'');
                    let path = self
                        .property("search_path", inner, true)
                        .unwrap_or_else(|| inner.to_owned());
                    format!("search_path = '{path}'")
                }
                _ => setting.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The namespace of string literals for [`Labels`].
const STRINGS: u8 = 5;

/// Replacements that keep what tells names apart and what makes them alike.
/// Names that differ only in their numbers share a base (`table_a`) and
/// get a number each (`table_a_1`, `table_a_2`); a name without numbers
/// gets a base of its own, without numbers.
#[derive(Default)]
struct Labels {
    /// The base of each name with its numbers left out, by namespace.
    bases: HashMap<(u8, String), String>,
    /// How many bases each prefix has.
    counts: HashMap<&'static str, usize>,
    /// How many names with numbers each base has.
    members: HashMap<String, usize>,
}

impl Labels {
    fn label(&mut self, namespace: u8, prefix: &'static str, original: &str) -> String {
        let pattern = blank_numbers(original);
        let base = match self.bases.get(&(namespace, pattern.clone())) {
            Some(base) => base.clone(),
            None => {
                let count = self.counts.entry(prefix).or_insert(0);
                *count += 1;
                let base = format!("{prefix}_{}", letters(*count));
                self.bases
                    .insert((namespace, pattern.clone()), base.clone());
                base
            }
        };
        if pattern == original {
            return base;
        }
        let member = self.members.entry(base.clone()).or_insert(0);
        *member += 1;
        format!("{base}_{member}")
    }
}

/// `a`, `b`, …, `z`, `aa`, `ab`, …: a count without digits.
fn letters(count: usize) -> String {
    let mut out = Vec::new();
    let mut n = count;
    while n > 0 {
        n -= 1;
        out.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

/// Splits `a = '1', b = 'x, y'` at the commas outside quotes.
fn split_settings(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (at, c) in value.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            ',' if !quoted && value[at + 1..].starts_with(' ') => {
                parts.push(&value[start..at]);
                start = at + 2;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// A name in a plan: as written, and the name it stands for.
#[derive(Debug)]
struct Identifier {
    raw: String,
    name: String,
    quoted: bool,
}

impl Identifier {
    /// The name a statement means: in lower case, unless quoted.
    fn folded(&self) -> Identifier {
        Identifier {
            raw: self.raw.clone(),
            name: if self.quoted {
                self.name.clone()
            } else {
                self.name.to_ascii_lowercase()
            },
            quoted: self.quoted,
        }
    }
}

fn is_identifier_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// The names of a dotted chain at the start of `text` (`a`, `t.col`,
/// `"My Schema".t`), and the text after it. Empty when no name starts
/// there.
fn identifier_chain(text: &str) -> (Vec<Identifier>, &str) {
    let mut chain = Vec::new();
    let mut rest = text;
    while let Some((identifier, after)) = identifier(rest) {
        chain.push(identifier);
        rest = after;
        match rest.strip_prefix('.') {
            Some(next) if next.starts_with(|c: char| c == '"' || is_identifier_start(c)) => {
                rest = next;
            }
            _ => break,
        }
    }
    (chain, rest)
}

fn identifier(text: &str) -> Option<(Identifier, &str)> {
    if text.starts_with('"') {
        let length = params::quoted(text, '"', false);
        let raw = &text[..length];
        let name = raw
            .strip_prefix('"')?
            .strip_suffix('"')?
            .replace("\"\"", "\"");
        return Some((
            Identifier {
                raw: raw.to_owned(),
                name,
                quoted: true,
            },
            &text[length..],
        ));
    }
    if !text.starts_with(is_identifier_start) {
        return None;
    }
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(text.len());
    Some((
        Identifier {
            raw: text[..end].to_owned(),
            name: text[..end].to_owned(),
            quoted: false,
        },
        &text[end..],
    ))
}

/// Copies the type after `::`, with its modifiers: `numeric(10,2)`,
/// `timestamp without time zone`, `integer[]`.
fn cast_type<'a>(text: &'a str, out: &mut String) -> &'a str {
    let (chain, mut rest) = identifier_chain(text);
    if chain.is_empty() {
        return text;
    }
    let raw: Vec<&str> = chain.iter().map(|part| part.raw.as_str()).collect();
    out.push_str(&raw.join("."));
    loop {
        let word = TYPE_WORDS.iter().find(|word| {
            rest.strip_prefix(' ')
                .and_then(|r| r.strip_prefix(*word))
                .is_some_and(|r| !r.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        });
        match word {
            Some(word) => {
                out.push(' ');
                out.push_str(word);
                rest = &rest[1 + word.len()..];
            }
            None => break,
        }
    }
    if rest.starts_with('(') {
        let end = rest.find(')').map_or(rest.len(), |end| end + 1);
        out.push_str(&rest[..end]);
        rest = &rest[end..];
    }
    while let Some(after) = rest.strip_prefix("[]") {
        out.push_str("[]");
        rest = after;
    }
    rest
}

/// The length of the number at the start of `text`: `42`, `1.5`, `1e-3`,
/// `1_000`.
fn number_length(text: &str) -> usize {
    let bytes = text.as_bytes();
    let digits = |at: usize| {
        bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b'_')
            .count()
    };
    let mut length = digits(0);
    if bytes.get(length) == Some(&b'.') {
        length += 1 + digits(length + 1);
    }
    if matches!(bytes.get(length), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(length + 1), Some(b'+' | b'-')));
        let exponent = digits(length + 1 + sign);
        if exponent > 0 {
            length += 1 + sign + exponent;
        }
    }
    length
}

/// The length of the JSON string at the start of `text`, quotes included.
fn json_string_length(text: &str) -> usize {
    let mut chars = text.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return at + 1,
            _ => {}
        }
    }
    text.len()
}

fn is_numbered(word: &str, prefix: &str) -> bool {
    word.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a line of a text plan is a node: it carries the node's costs or
/// its actual figures.
fn is_node_line(content: &str) -> bool {
    content.contains(" (cost=")
        || content.contains(" (actual ")
        || content.ends_with("(never executed)")
}

/// Whether a line of a text plan names a node in PostgreSQL's words, as
/// plans without costs print them: `Hash Join`, `Seq Scan on orders o`,
/// `Custom Scan (ChunkAppend) on metrics`.
fn is_node_name(content: &str) -> bool {
    let node_type = content
        .find(" using ")
        .or_else(|| content.find(" on "))
        .map_or(content, |at| &content[..at]);
    // A custom scan's provider is the extension's name.
    let node_type = match node_type.split_once("Custom Scan (") {
        Some((before, provider)) => match provider.split_once(')') {
            Some((_, after)) => format!("{before}Custom Scan{after}"),
            None => return false,
        },
        None => node_type.to_owned(),
    };
    !node_type.is_empty() && node_type.split(' ').all(|word| NODE_WORDS.contains(&word))
}

/// Whether the label of a text plan's line is in PostgreSQL's words:
/// `Rows Removed by Filter`, `I/O Timings`, `Full-sort Groups`.
fn is_label(label: &str) -> bool {
    !label.is_empty()
        && label.split(' ').all(|word| {
            matches!(word, "by" | "for" | "in" | "of" | "per" | "to")
                || (word.starts_with(|c: char| c.is_ascii_uppercase())
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphabetic() || c == '/' || c == '-'))
        })
}

/// A worker's first line: `actual time=0.1..0.2 rows=10 loops=1`.
fn is_actual(content: &str) -> bool {
    content.strip_prefix("actual ").is_some_and(|figures| {
        figures.split_whitespace().all(|figure| {
            figure.split_once('=').is_some_and(|(key, value)| {
                matches!(key, "time" | "rows" | "loops")
                    && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            })
        })
    })
}

/// A keyword as PostgreSQL prints it in a plan: in capitals, since it
/// quotes names with capitals. In a statement someone wrote, in any case.
fn is_keyword(word: &str, sql: bool) -> bool {
    if matches!(word, "true" | "false") {
        return true;
    }
    if sql {
        KEYWORDS
            .iter()
            .chain(SQL_KEYWORDS)
            .any(|k| k.eq_ignore_ascii_case(word))
    } else {
        KEYWORDS
            .iter()
            .chain(SQL_KEYWORDS)
            .chain(PRINTED)
            .any(|k| *k == word)
    }
}

/// A keyword that names nothing unless quoted.
fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|k| k.eq_ignore_ascii_case(word))
}

/// Properties whose value is an expression or a list of them.
const EXPRESSIONS: &[&str] = &[
    "Output",
    "Sort Key",
    "Presorted Key",
    "Group Key",
    "Group Keys",
    "Hash Key",
    "Hash Keys",
    "Sort Keys",
    "Partition Key",
    "Filter",
    "Index Cond",
    "Recheck Cond",
    "Join Filter",
    "Hash Cond",
    "Merge Cond",
    "One-Time Filter",
    "Order By",
    "TID Cond",
    "Run Condition",
    "Cache Key",
    "Function Call",
    "Table Function Call",
    "Window",
    "Relations",
    "Conflict Filter",
    "Sampling Parameters",
    "Repeatable Seed",
];

/// Properties whose values are figures, settings or PostgreSQL's words,
/// and name nothing: labels of text plans and keys of JSON plans.
const KEPT: &[&str] = &[
    "Node Type",
    "Parent Relationship",
    "Join Type",
    "Strategy",
    "Partial Mode",
    "Operation",
    "Command",
    "Scan Direction",
    "Sort Space Type",
    "Sort Methods Used",
    "Function Name",
    "Table Function Name",
    "Custom Plan Provider",
    "Sampling Method",
    "Format",
    "Buffers",
    "I/O Timings",
    "Planning Time",
    "Execution Time",
    "Total runtime",
    "Planning",
    "Rows Removed by Filter",
    "Rows Removed by Join Filter",
    "Rows Removed by Index Recheck",
    "Rows Removed by Conflict Filter",
    "Heap Fetches",
    "Heap Blocks",
    "Sort Method",
    "Sort Space Used",
    "Inner Unique",
    "Single Copy",
    "Disabled",
    "Batches",
    "Buckets",
    "Planned Partitions",
    "Workers Planned",
    "Workers Launched",
    "Index Searches",
    "Subplans Removed",
    "Tuples Inserted",
    "Conflicting Tuples",
    "Conflict Resolution",
    "Timing",
    "Options",
    "Functions",
    "JIT",
    "Storage",
    "Memory",
    "WAL",
    "Full-sort Groups",
    "Pre-sorted Groups",
    "Hits",
    "Cache Mode",
    "Serialization",
    "Query Identifier",
    "Params Evaluated",
];

/// The words of node types in text plans.
const NODE_WORDS: &[&str] = &[
    "Aggregate",
    "All",
    "Anti",
    "Append",
    "Async",
    "Backward",
    "Bitmap",
    "BitmapAnd",
    "BitmapOr",
    "CTE",
    "Custom",
    "Delete",
    "Except",
    "Finalize",
    "Foreign",
    "Full",
    "Function",
    "Gather",
    "Group",
    "GroupAggregate",
    "Hash",
    "HashAggregate",
    "HashSetOp",
    "Heap",
    "Incremental",
    "Index",
    "Inner",
    "Insert",
    "Intersect",
    "Join",
    "Left",
    "Limit",
    "LockRows",
    "Loop",
    "Materialize",
    "Memoize",
    "Merge",
    "MixedAggregate",
    "Named",
    "Nested",
    "Only",
    "Parallel",
    "Partial",
    "ProjectSet",
    "Range",
    "Recursive",
    "Result",
    "Right",
    "Sample",
    "Scan",
    "Semi",
    "Seq",
    "SetOp",
    "Sort",
    "Subquery",
    "Table",
    "Tid",
    "Tuplestore",
    "Union",
    "Unique",
    "Update",
    "Values",
    "WindowAgg",
    "WorkTable",
];

/// Columns every table has.
const SYSTEM_COLUMNS: &[&str] = &["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// Whether a word is one of the plan's own, by what follows it:
/// `(hashed SubPlan 2)`, `InitPlan 1 (returns $0)`.
fn is_plan_word(word: &str, after: &str) -> bool {
    match word {
        "InitPlan" | "SubPlan" => after
            .strip_prefix(' ')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit())),
        "hashed" => after.starts_with(" SubPlan "),
        // `(alternatives: SubPlan 1 or hashed SubPlan 2)`, before
        // PostgreSQL 14.
        "alternatives" => after.starts_with(": SubPlan ") || after.starts_with(": hashed SubPlan "),
        "or" => after.starts_with(" SubPlan ") || after.starts_with(" hashed SubPlan "),
        "returns" => after.starts_with(" $"),
        "CTE" => after.starts_with(' '),
        _ => false,
    }
}

/// Words that continue a type name after `::`.
const TYPE_WORDS: &[&str] = &["without", "with", "time", "zone", "varying", "precision"];

/// Keywords PostgreSQL prints in conditions and output columns.
const KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "AT",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COLLATE",
    "COLLATION",
    "CURRENT",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "DESC",
    "DISTINCT",
    "ELSE",
    "END",
    "EXCLUDE",
    "EXISTS",
    "FALSE",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FROM",
    "GROUP",
    "GROUPS",
    "ILIKE",
    "IN",
    "INTERVAL",
    "IS",
    "LAST",
    "LIKE",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NO",
    "NOT",
    "NULL",
    "NULLS",
    "OPERATOR",
    "OR",
    "ORDER",
    "OTHERS",
    "OVER",
    "OVERLAPS",
    "PARTITION",
    "PLACING",
    "PRECEDING",
    "RANGE",
    "REPEATABLE",
    "ROW",
    "ROWS",
    "SESSION_USER",
    "SIMILAR",
    "SOME",
    "SYMMETRIC",
    "SYSTEM_USER",
    "THEN",
    "TIES",
    "TIME",
    "TO",
    "TRUE",
    "UNBOUNDED",
    "UNIQUE",
    "UNKNOWN",
    "USER",
    "USING",
    "VARIADIC",
    "WHEN",
    "WITHIN",
    "ZONE",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "JOIN",
    "ON",
    "CROSS",
];

/// More keywords, of statements.
const SQL_KEYWORDS: &[&str] = &[
    "ABORT",
    "ALTER",
    "ANALYZE",
    "ASYMMETRIC",
    "BEGIN",
    "BOTH",
    "BUFFERS",
    "CALL",
    "CHECK",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "COPY",
    "COSTS",
    "CREATE",
    "CUBE",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DO",
    "DROP",
    "EXCEPT",
    "EXECUTE",
    "EXPLAIN",
    "FETCH",
    "FOR",
    "FORMAT",
    "GENERIC_PLAN",
    "GROUPING",
    "HAVING",
    "INSERT",
    "INTERSECT",
    "INTO",
    "JSON",
    "LATERAL",
    "LEADING",
    "LIMIT",
    "LOCKED",
    "MATERIALIZED",
    "MERGE",
    "NATURAL",
    "NOTHING",
    "NOWAIT",
    "OF",
    "OFFSET",
    "ONLY",
    "PREPARE",
    "RECURSIVE",
    "RETURNING",
    "ROLLBACK",
    "ROLLUP",
    "SELECT",
    "SET",
    "SETS",
    "SETTINGS",
    "SHARE",
    "SKIP",
    "TABLE",
    "TABLESAMPLE",
    "TIMING",
    "TRAILING",
    "UNION",
    "UPDATE",
    "VALUES",
    "VERBOSE",
    "WAL",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
    "MATCHED",
    "UPSERT",
];

/// Words PostgreSQL prints in some conditions and output columns, kept in
/// plans only: `PARTIAL count(*)`, `IS JSON SCALAR`, `JSON_VALUE(… ERROR
/// ON ERROR)`. A statement may well name a column `value` or `partial`.
const PRINTED: &[&str] = &[
    "ABSENT",
    "CONDITIONAL",
    "EMPTY",
    "ENCODING",
    "ERROR",
    "ESCAPE",
    "KEEP",
    "KEYS",
    "LOCAL",
    "NFC",
    "NFD",
    "NFKC",
    "NFKD",
    "NORMALIZED",
    "OBJECT",
    "OMIT",
    "PARTIAL",
    "PASSING",
    "QUOTES",
    "SCALAR",
    "UNCONDITIONAL",
    "UTF8",
    "VALUE",
    "WRAPPER",
];

/// The keywords above that name nothing unless quoted: PostgreSQL's
/// reserved keywords, and those that may only name a function or a type.
const RESERVED: &[&str] = &[
    "ALL",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASYMMETRIC",
    "BOTH",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLLATION",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "DESC",
    "DISTINCT",
    "DO",
    "ELSE",
    "END",
    "EXCEPT",
    "FALSE",
    "FETCH",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INNER",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LATERAL",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NATURAL",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVERLAPS",
    "PLACING",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SESSION_USER",
    "SIMILAR",
    "SOME",
    "SYMMETRIC",
    "SYSTEM_USER",
    "TABLE",
    "TABLESAMPLE",
    "THEN",
    "TO",
    "TRAILING",
    "TRUE",
    "UNION",
    "UNIQUE",
    "USER",
    "USING",
    "VARIADIC",
    "VERBOSE",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn text(plan: &str) -> String {
        anonymize(plan, Options::default()).unwrap().text
    }

    #[test]
    fn replaces_names_and_values_in_a_text_plan() {
        let plan = "\
Hash Join  (cost=1.00..2.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
  Hash Cond: (o.customer_id = c.id)
  ->  Seq Scan on public.orders o  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
        Filter: ((o.status = 'shipped'::text) AND (o.note ~~ 'gift%'::text) AND (o.amount > 100.5))
        Rows Removed by Filter: 10
  ->  Hash  (cost=1.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
        ->  Index Scan using customers_pkey on public.customers c  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
              Index Cond: (c.id = $1)
              Filter: (lower(c.email) = 'a@example.com'::character varying)
Trigger RI_ConstraintTrigger_a_16417 for constraint orders_customer_id_fkey: time=0.5 calls=1
Settings: work_mem = '64MB', search_path = 'app, public'
Execution Time: 0.3 ms";
        insta::assert_snapshot!(text(plan), @r"
        Hash Join  (cost=1.00..2.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
          Hash Cond: (alias_a.column_a = alias_b.column_b)
          ->  Seq Scan on public.table_a alias_a  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                Filter: ((alias_a.column_c = 'value_a'::text) AND (alias_a.column_d ~~ 'value_b%'::text) AND (alias_a.column_e > 1.0))
                Rows Removed by Filter: 10
          ->  Hash  (cost=1.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                ->  Index Scan using index_a on public.table_b alias_b  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                      Index Cond: (alias_b.column_b = $1)
                      Filter: (lower(alias_b.column_f) = 'value_c'::character varying)
        Trigger RI_ConstraintTrigger_a_16417 for constraint constraint_a: time=0.5 calls=1
        Settings: work_mem = '64MB', search_path = 'schema_a, public'
        Execution Time: 0.3 ms
        ");
    }

    #[test]
    fn rewrites_json_values_and_keeps_the_layout() {
        let plan = r#"[
  {
    "Plan": {
      "Node Type": "Seq Scan",
      "Relation Name": "orders",
      "Alias": "o",
      "Startup Cost": 0.00,
      "Total Cost": 1.00,
      "Plan Rows": 1,
      "Plan Width": 4,
      "Output": ["o.id", "o.\"Note\""],
      "Filter": "(o.status = 'shipped'::text)"
    },
    "Query Text": "SELECT o.id, o.\"Note\" FROM orders o WHERE o.status = 'shipped' -- route: /admin"
  }
]"#;
        insta::assert_snapshot!(text(plan), @r#"
        [
          {
            "Plan": {
              "Node Type": "Seq Scan",
              "Relation Name": "table_a",
              "Alias": "alias_a",
              "Startup Cost": 0.00,
              "Total Cost": 1.00,
              "Plan Rows": 1,
              "Plan Width": 4,
              "Output": ["alias_a.column_a", "alias_a.column_b"],
              "Filter": "(alias_a.column_c = 'value_a'::text)"
            },
            "Query Text": "SELECT alias_a.column_a, alias_a.column_b FROM table_a alias_a WHERE alias_a.column_c = 'value_a'  "
          }
        ]
        "#);
    }

    #[test]
    fn keeps_names_when_asked() {
        let plan = "Seq Scan on orders o  (cost=0.00..1.00 rows=1 width=4)\n  Filter: (o.customer_id = 4242)";
        let anonymized = anonymize(plan, Options { keep_names: true }).unwrap();
        assert_eq!(
            anonymized.text,
            "Seq Scan on orders o  (cost=0.00..1.00 rows=1 width=4)\n  Filter: (o.customer_id = 1)\n"
        );
        assert_eq!(
            anonymized.mapping.values,
            BTreeMap::from([("4242".to_owned(), "1".to_owned())])
        );
    }

    #[test]
    fn keeps_plan_words_types_and_functions() {
        let plan = "\
Seq Scan on events e  (cost=0.00..1.00 rows=1 width=4)
  Filter: ((e.created_at >= (InitPlan 1).col1) AND (date_trunc('day'::text, e.created_at) = '2024-01-01 00:00:00'::timestamp without time zone) AND (e.kind = ANY ('{a,b}'::text[])) AND (NOT (hashed SubPlan 2)) AND (e.amount = '1.50'::numeric(10,2)) AND ((e.name)::text = E'it\\'s'::text COLLATE \"C\"))";
        insta::assert_snapshot!(text(plan), @r#"
        Seq Scan on table_a alias_a  (cost=0.00..1.00 rows=1 width=4)
          Filter: ((alias_a.column_a >= (InitPlan 1).col1) AND (date_trunc('value_a'::text, alias_a.column_a) = 'value_b_1'::timestamp without time zone) AND (alias_a.column_b = ANY ('value_c'::text[])) AND (NOT (hashed SubPlan 2)) AND (alias_a.column_c = 'value_d_1'::numeric(10,2)) AND ((alias_a.column_d)::text = 'value_e'::text COLLATE "C"))
        "#);
    }

    #[test]
    fn keeps_what_aggregates_windows_and_subplans_print() {
        let plan = "\
Aggregate  (cost=1.00..2.00 rows=1 width=8)
  Output: PARTIAL count(*) FILTER (WHERE (o.amount > '10'::numeric)), row_number() OVER w1
  Window: w1 AS (PARTITION BY o.status ORDER BY o.created_at ROWS UNBOUNDED PRECEDING)
  ->  Seq Scan on public.orders o  (cost=0.00..1.00 rows=1 width=4)
        Filter: (alternatives: SubPlan 1 or hashed SubPlan 2)";
        insta::assert_snapshot!(text(plan), @r"
        Aggregate  (cost=1.00..2.00 rows=1 width=8)
          Output: PARTIAL count(*) FILTER (WHERE (alias_a.column_a > 'value_a_1'::numeric)), row_number() OVER w1
          Window: w1 AS (PARTITION BY alias_a.column_b ORDER BY alias_a.column_c ROWS UNBOUNDED PRECEDING)
          ->  Seq Scan on public.table_a alias_a  (cost=0.00..1.00 rows=1 width=4)
                Filter: (alternatives: SubPlan 1 or hashed SubPlan 2)
        ");
    }

    /// The statement of an auto_explain entry names what its plan names the
    /// same way: unquoted names in any case, and columns named by keywords.
    #[test]
    fn names_a_statement_as_its_plan_does() {
        let plan = r#"{"Query Text": "SELECT Time, first FROM Metrics m WHERE m.Time > now() - interval '1 day' ORDER BY Time DESC NULLS LAST", "Plan": {"Node Type": "Seq Scan", "Relation Name": "metrics", "Alias": "m", "Startup Cost": 0.00, "Total Cost": 1.00, "Plan Rows": 1, "Plan Width": 4, "Output": ["\"time\"", "first"], "Filter": "(m.\"time\" > (now() - '1 day'::interval))"}}"#;
        insta::assert_snapshot!(text(plan), @r#"{"Query Text": "SELECT column_a, column_b FROM table_a alias_a WHERE alias_a.column_a > now() - interval 'value_a_1' ORDER BY column_a DESC NULLS LAST", "Plan": {"Node Type": "Seq Scan", "Relation Name": "table_a", "Alias": "alias_a", "Startup Cost": 0.00, "Total Cost": 1.00, "Plan Rows": 1, "Plan Width": 4, "Output": ["column_a", "column_b"], "Filter": "(alias_a.column_a > (now() - 'value_a_1'::interval))"}}"#);
    }

    #[test]
    fn keeps_every_reserved_keyword() {
        for word in RESERVED {
            assert!(is_keyword(word, true), "{word}");
        }
    }

    #[test]
    fn names_the_same_thing_the_same_way_in_every_plan() {
        let plans = "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n  Filter: (status = 'new'::text)\n\nSeq Scan on orders  (cost=0.00..2.00 rows=1 width=4)\n  Filter: (status = 'new'::text)";
        let anonymized = text(plans);
        assert_eq!(anonymized.matches("table_a").count(), 2, "{anonymized}");
        assert_eq!(anonymized.matches("'value_a'").count(), 2, "{anonymized}");
    }

    #[test]
    fn leaves_out_what_surrounds_a_plan() {
        let input = "mydb=# EXPLAIN SELECT * FROM secret_table;\n                      QUERY PLAN\n------------------------------------------------------\n Seq Scan on secret_table  (cost=0.00..1.00 rows=1 width=4)\n(1 row)\n";
        let anonymized = text(input);
        assert!(!anonymized.contains("secret"), "{anonymized}");
        assert!(!anonymized.contains("mydb"), "{anonymized}");
    }

    #[test]
    fn keeps_partitions_alike_and_other_names_apart() {
        let plan = "\
Append  (cost=0.00..2.00 rows=2 width=4)
  ->  Seq Scan on events_2025_01 events_1  (cost=0.00..1.00 rows=1 width=4)
        Filter: (events_1.kind = 'click'::text)
  ->  Seq Scan on events_2025_02 events_2  (cost=0.00..1.00 rows=1 width=4)
        Filter: (events_2.kind = 'click'::text)
  ->  Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)
        Filter: (orders.status = 'new'::text)";
        insta::assert_snapshot!(text(plan), @r"
        Append  (cost=0.00..2.00 rows=2 width=4)
          ->  Seq Scan on table_a_1 alias_a_1  (cost=0.00..1.00 rows=1 width=4)
                Filter: (alias_a_1.column_a = 'value_a'::text)
          ->  Seq Scan on table_a_2 alias_a_2  (cost=0.00..1.00 rows=1 width=4)
                Filter: (alias_a_2.column_a = 'value_a'::text)
          ->  Seq Scan on table_b  (cost=0.00..1.00 rows=1 width=4)
                Filter: (table_b.column_b = 'value_b'::text)
        ");
    }

    #[test]
    fn counts_in_letters() {
        let counted: Vec<String> = [1, 2, 26, 27, 52, 53, 702, 703].map(letters).into();
        assert_eq!(counted, ["a", "b", "z", "aa", "az", "ba", "zz", "aaa"]);
    }

    #[test]
    fn replaces_what_it_does_not_know_in_json() {
        let plan = r#"{
  "Query Text": "select * from orders where id = $1",
  "Query Parameters": "$1 = '4242'",
  "Plan": {
    "Node Type": "Seq Scan",
    "Relation Name": "orders",
    "Alias": "orders",
    "Startup Cost": 0.00,
    "Total Cost": 1.00,
    "Plan Rows": 1,
    "Plan Width": 4,
    "Something New": "orders.secret"
  },
  "Settings": {"enable_seqscan": "off", "search_path": "app, public"}
}"#;
        insta::assert_snapshot!(text(plan), @r#"
        {
          "Query Text": "select * from table_a where column_a = $1",
          "Query Parameters": "$1 = 'value_a_1'",
          "Plan": {
            "Node Type": "Seq Scan",
            "Relation Name": "table_a",
            "Alias": "table_a",
            "Startup Cost": 0.00,
            "Total Cost": 1.00,
            "Plan Rows": 1,
            "Plan Width": 4,
            "Something New": "table_a.column_b"
          },
          "Settings": {"enable_seqscan": "off", "search_path": "schema_a, public"}
        }
        "#);
    }

    #[test]
    fn replaces_names_in_lines_it_does_not_place() {
        // The target tables of an UPDATE of a partitioned table, and a
        // prompt pasted after the plan.
        let plan = "\
Update on parted  (cost=0.00..2.00 rows=0 width=0)
  Update on parted_p1 parted_1
  ->  Seq Scan on parted_p1 parted_1  (cost=0.00..1.00 rows=1 width=10)
        Filter: (secret = 1)
mydb=# select secret from parted;";
        insta::assert_snapshot!(text(plan), @r"
        Update on table_a  (cost=0.00..2.00 rows=0 width=0)
          Update on table_b_1 alias_a_1
          ->  Seq Scan on table_b_1 alias_a_1  (cost=0.00..1.00 rows=1 width=10)
                Filter: (column_a = 1)
        column_b=# select column_a from table_a;
        ");
    }

    #[test]
    fn fences_plans_of_both_formats() {
        let input = "```json\n[{\"Plan\": {\"Node Type\": \"Seq Scan\", \"Relation Name\": \"orders\", \"Alias\": \"orders\"}}]\n```\n\n```\nSeq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n```";
        insta::assert_snapshot!(text(input), @r#"
        ```
        [{"Plan": {"Node Type": "Seq Scan", "Relation Name": "table_a", "Alias": "table_a"}}]
        ```

        ```
        Seq Scan on table_a  (cost=0.00..1.00 rows=1 width=4)
        ```
        "#);
    }

    #[test]
    fn refuses_an_input_without_a_plan() {
        assert_eq!(
            anonymize("hello", Options::default()).unwrap_err(),
            Error::Parse(ParseError::NoPlan)
        );
    }
}
