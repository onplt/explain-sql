//! Anonymizing plans so that they can be shared: in a bug report, an issue
//! or a chat with someone outside the team.
//!
//! The plan keeps its shape, its numbers and its node types, so that it
//! reads and analyzes as before. What could tell about the data or the
//! schema is replaced, the same way everywhere it appears:
//!
//! - the names of tables, indexes, CTEs, aliases, schemas, columns,
//!   constraints and triggers (`table1`, `index1`, `column1`, …);
//! - literal values, strings as `'value1'` (keeping the `%` of a `LIKE`
//!   pattern at either end) and numbers as small integers.
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
//!     "Seq Scan on table1 alias1  (cost=0.00..4917.00 rows=10 width=64)\n  Filter: (alias1.column1 = 1)\n",
//! );
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::Serialize;

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
        parts.push(text.trim_end().to_owned());
    }
    let mut text = parts.join("\n\n");
    text.push('\n');
    match pg::parse_all(&text) {
        Ok(again) if again.len() == plans.len() => Ok(Anonymized {
            text,
            mapping: namer.mapping,
        }),
        _ => Err(Error::Unreadable),
    }
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
    counts: HashMap<Kind, usize>,
    strings: HashMap<String, String>,
    numbers: HashMap<String, String>,
    mapping: Mapping,
}

impl Namer {
    fn new(options: Options) -> Self {
        Namer {
            options,
            names: HashMap::new(),
            counts: HashMap::new(),
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
        let count = self.counts.entry(kind).or_insert(0);
        *count += 1;
        let name = format!("{}{count}", kind.prefix());
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

    /// `'value1'`, keeping the `%` of a `LIKE` pattern at either end.
    fn string(&mut self, content: &str) -> String {
        let count = self.strings.len() + 1;
        let name = self
            .strings
            .entry(content.to_owned())
            .or_insert_with(|| format!("value{count}"))
            .clone();
        self.mapping
            .values
            .insert(format!("'{content}'"), format!("'{name}'"));
        let start = if content.starts_with('%') { "%" } else { "" };
        let end = if content.len() > 1 && content.ends_with('%') {
            "%"
        } else {
            ""
        };
        format!("'{start}{name}{end}'")
    }

    fn number(&mut self, text: &str) -> String {
        let count = self.numbers.len() + 1;
        let number = self
            .numbers
            .entry(text.to_owned())
            .or_insert_with(|| count.to_string())
            .clone();
        self.mapping.values.insert(text.to_owned(), number.clone());
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
            } else if (c == '"' || is_identifier_start(c)) && !previous_word {
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
        let raw = || {
            chain
                .iter()
                .map(|part| part.raw.as_str())
                .collect::<Vec<_>>()
                .join(".")
        };
        // Functions keep their names: count(*), lower(email).
        if after.starts_with('(') && !chain.last().is_some_and(|part| part.quoted) {
            return raw();
        }
        if keyword.eq_ignore_ascii_case("COLLATE") {
            return raw();
        }
        match chain {
            [word] => {
                if !word.quoted
                    && (is_keyword(&word.raw, sql) || PLAN_WORDS.contains(&word.raw.as_str()))
                {
                    return word.raw.clone();
                }
                // (InitPlan 1).col1, a window w1.
                if (before.ends_with(").") && is_numbered(&word.raw, "col"))
                    || (is_numbered(&word.raw, "w") && after.starts_with(" AS ("))
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
        let mut out = String::with_capacity(text.len());
        // For each open object, the key being read; arrays repeat the key
        // that holds them.
        let mut stack: Vec<(bool, String)> = Vec::new();
        let mut expecting_key = false;
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            match c {
                '{' => {
                    stack.push((true, String::new()));
                    expecting_key = true;
                }
                '[' => {
                    let key = stack.last().map(|(_, key)| key.clone()).unwrap_or_default();
                    stack.push((false, key));
                    expecting_key = false;
                }
                '}' | ']' => {
                    stack.pop();
                    expecting_key = false;
                }
                ',' => expecting_key = stack.last().is_some_and(|(object, _)| *object),
                ':' => expecting_key = false,
                '"' => {
                    let length = json_string_length(rest);
                    let token = &rest[..length];
                    let value: Option<String> = serde_json::from_str(token).ok();
                    rest = &rest[length..];
                    match (value, stack.last_mut()) {
                        (Some(value), Some((true, key))) if expecting_key => {
                            *key = value;
                            out.push_str(token);
                        }
                        (Some(value), Some((_, key))) => {
                            let key = key.clone();
                            match self.property(&key, &value, false) {
                                Some(new) => out.push_str(
                                    &serde_json::to_string(&new).expect("a string serializes"),
                                ),
                                None => out.push_str(token),
                            }
                        }
                        _ => out.push_str(token),
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
            _ if EXPRESSIONS.contains(&key) => self.expression(value, false),
            _ if text && !KEPT.contains(&key) => {
                // An unfamiliar label: its value could hold anything.
                self.expression(value, true)
            }
            _ => return None,
        };
        Some(new)
    }

    /// Rewrites a text plan line by line.
    fn text_plan(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut seen_node = false;
        for line in text.split_inclusive('\n') {
            let (line, newline) = match line.strip_suffix('\n') {
                Some(line) => (line, "\n"),
                None => (line, ""),
            };
            let content = line.trim_start();
            out.push_str(&line[..line.len() - content.len()]);
            out.push_str(&self.text_line(content, &mut seen_node));
            out.push_str(newline);
        }
        out
    }

    fn text_line(&mut self, content: &str, seen_node: &mut bool) -> String {
        if content.is_empty() {
            return String::new();
        }
        if let Some(header) = content.strip_prefix("->") {
            let name = header.trim_start();
            *seen_node = true;
            return format!(
                "->{}{}",
                &header[..header.len() - name.len()],
                self.node_header(name)
            );
        }
        if is_node_line(content) || !*seen_node && !content.contains(": ") {
            *seen_node = true;
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
                let property = self.text_line(property, seen_node);
                return format!("{worker}:{space}{property}");
            }
        }
        if let Some((label, value)) = content.split_once(": ") {
            if label == "Settings" {
                return format!("Settings: {}", self.settings(value));
            }
            return match self.property(label, value, true) {
                Some(value) => format!("{label}: {value}"),
                None => content.to_owned(),
            };
        }
        if content.ends_with(':')
            || content.starts_with("InitPlan ")
            || content.starts_with("SubPlan ")
            || content.starts_with("actual ")
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
        let mut out = String::new();
        let mut rest = name;
        if let Some(at) = rest.find(" using ") {
            out.push_str(&rest[..at + " using ".len()]);
            let node_type = &rest[..at];
            let (chain, after) = identifier_chain(&rest[at + " using ".len()..]);
            match chain.as_slice() {
                [index] => out.push_str(&self.written(Kind::Index, index)),
                _ => return format!("{}{numbers}", self.expression(name, true)),
            }
            rest = after;
            if let Some(target) = rest.strip_prefix(" on ") {
                out.push_str(" on ");
                out.push_str(&self.target(node_type, target));
                rest = "";
            }
        } else if let Some(at) = rest.find(" on ") {
            out.push_str(&rest[..at + " on ".len()]);
            out.push_str(&self.target(&rest[..at], &rest[at + " on ".len()..]));
            rest = "";
        }
        out.push_str(rest);
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

fn number_length(text: &str) -> usize {
    let mut length = 0;
    let bytes = text.as_bytes();
    while length < bytes.len() {
        let b = bytes[length];
        let exponent_sign =
            (b == b'+' || b == b'-') && length > 0 && matches!(bytes[length - 1], b'e' | b'E');
        if b.is_ascii_alphanumeric() || b == b'.' || exponent_sign {
            length += 1;
        } else {
            break;
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

/// A keyword as PostgreSQL prints it in a plan: in capitals. In a
/// statement someone wrote, in any case.
fn is_keyword(word: &str, sql: bool) -> bool {
    if sql {
        KEYWORDS
            .iter()
            .chain(SQL_KEYWORDS)
            .any(|k| k.eq_ignore_ascii_case(word))
            || matches!(word, "true" | "false")
    } else {
        KEYWORDS.contains(&word) || matches!(word, "true" | "false")
    }
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
    "Sampling",
    "Repeatable Seed",
];

/// Labels of a text plan whose values are figures or settings, and name
/// nothing.
const KEPT: &[&str] = &[
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
    "Estimates",
    "Prefetch",
    "Window Aggregate",
    "Peak Memory Usage",
    "Average Prefetch Distance",
];

/// Columns every table has.
const SYSTEM_COLUMNS: &[&str] = &["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// Words of plans that are not names: `(hashed SubPlan 2)`,
/// `InitPlan 1 (returns $0)`.
const PLAN_WORDS: &[&str] = &["InitPlan", "SubPlan", "hashed", "returns", "CTE"];

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
    "CURRENT",
    "CURRENT_DATE",
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
    "PARTITION",
    "PRECEDING",
    "RANGE",
    "ROW",
    "ROWS",
    "SESSION_USER",
    "SIMILAR",
    "SOME",
    "SYMMETRIC",
    "THEN",
    "TIES",
    "TIME",
    "TO",
    "TRUE",
    "UNBOUNDED",
    "UNKNOWN",
    "USER",
    "USING",
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
    "KEY",
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
    "TEXT",
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
          Hash Cond: (alias1.column1 = alias2.column2)
          ->  Seq Scan on public.table1 alias1  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                Filter: ((alias1.column3 = 'value1'::text) AND (alias1.column4 ~~ 'value2%'::text) AND (alias1.column5 > 1))
                Rows Removed by Filter: 10
          ->  Hash  (cost=1.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                ->  Index Scan using index1 on public.table2 alias2  (cost=0.00..1.00 rows=1 width=4) (actual time=0.1..0.2 rows=1 loops=1)
                      Index Cond: (alias2.column2 = $1)
                      Filter: (lower(alias2.column6) = 'value3'::character varying)
        Trigger RI_ConstraintTrigger_a_16417 for constraint constraint1: time=0.5 calls=1
        Settings: work_mem = '64MB', search_path = 'schema1, public'
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
              "Relation Name": "table1",
              "Alias": "alias1",
              "Startup Cost": 0.00,
              "Total Cost": 1.00,
              "Plan Rows": 1,
              "Plan Width": 4,
              "Output": ["alias1.column1", "alias1.column2"],
              "Filter": "(alias1.column3 = 'value1'::text)"
            },
            "Query Text": "SELECT alias1.column1, alias1.column2 FROM table1 alias1 WHERE alias1.column3 = 'value1'  "
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
        Seq Scan on table1 alias1  (cost=0.00..1.00 rows=1 width=4)
          Filter: ((alias1.column1 >= (InitPlan 1).col1) AND (date_trunc('value1'::text, alias1.column1) = 'value2'::timestamp without time zone) AND (alias1.column2 = ANY ('value3'::text[])) AND (NOT (hashed SubPlan 2)) AND (alias1.column3 = 'value4'::numeric(10,2)) AND ((alias1.column4)::text = 'value5'::text COLLATE "C"))
        "#);
    }

    #[test]
    fn names_the_same_thing_the_same_way_in_every_plan() {
        let plans = "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n  Filter: (status = 'new'::text)\n\nSeq Scan on orders  (cost=0.00..2.00 rows=1 width=4)\n  Filter: (status = 'new'::text)";
        let anonymized = text(plans);
        assert_eq!(anonymized.matches("table1").count(), 2, "{anonymized}");
        assert_eq!(anonymized.matches("'value1'").count(), 2, "{anonymized}");
    }

    #[test]
    fn leaves_out_what_surrounds_a_plan() {
        let input = "mydb=# EXPLAIN SELECT * FROM secret_table;\n                      QUERY PLAN\n------------------------------------------------------\n Seq Scan on secret_table  (cost=0.00..1.00 rows=1 width=4)\n(1 row)\n";
        let anonymized = text(input);
        assert!(!anonymized.contains("secret"), "{anonymized}");
        assert!(!anonymized.contains("mydb"), "{anonymized}");
    }

    #[test]
    fn refuses_an_input_without_a_plan() {
        assert_eq!(
            anonymize("hello", Options::default()).unwrap_err(),
            Error::Parse(ParseError::NoPlan)
        );
    }
}
