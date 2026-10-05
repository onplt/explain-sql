//! What the database says about the tables in a plan: their size, indexes,
//! column collations and statistics, foreign keys and the installed
//! extensions. Read in connected mode by `explainsql-db`; plain data here so
//! that the advisor can use it without I/O.

use serde::Serialize;

use crate::advisor::{Advice, AdviceKind};
use crate::ir::Plan;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Catalog {
    pub tables: Vec<Table>,
    pub foreign_keys: Vec<ForeignKey>,
    /// Installed extensions: `pg_trgm`, `hypopg`, …
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub partitioned: bool,
    /// From `pg_class`, as of the last `VACUUM` or `ANALYZE`.
    pub pages: f64,
    pub rows: f64,
    /// Including indexes and TOAST.
    pub total_bytes: i64,
    /// When the table was last analyzed, by hand or by autovacuum.
    pub last_analyzed: Option<String>,
    pub indexes: Vec<ExistingIndex>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExistingIndex {
    pub name: String,
    /// `CREATE INDEX … ON … USING btree (…)`.
    pub definition: String,
    /// `btree`, `gin`, …
    pub method: String,
    /// The key columns or expressions, in order.
    pub columns: Vec<String>,
    pub unique: bool,
    /// False for an index a failed `CREATE INDEX CONCURRENTLY` left behind.
    pub valid: bool,
    pub partial: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Column {
    pub name: String,
    pub collation: Option<String>,
    /// From `pg_stats`: distinct values, or minus their share of the rows.
    pub n_distinct: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ForeignKey {
    pub constraint: String,
    /// The referencing table and columns.
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
}

impl Catalog {
    /// A table by name, in the given schema or any.
    pub fn table(&self, schema: Option<&str>, name: &str) -> Option<&Table> {
        self.tables
            .iter()
            .find(|table| table.name == name && schema.is_none_or(|schema| table.schema == schema))
    }
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// What to read for a plan and its advice: the tables (schema if known,
/// and name) and the foreign-key constraints.
pub fn wanted(plan: &Plan, advice: &[Advice]) -> (Vec<(Option<String>, String)>, Vec<String>) {
    let mut tables: Vec<(Option<String>, String)> = Vec::new();
    let mut add = |schema: Option<String>, name: String| {
        if !tables.iter().any(|(s, n)| *s == schema && *n == name) {
            tables.push((schema, name));
        }
    };
    for node in &plan.nodes {
        if let Some(name) = &node.relation_name {
            add(node.schema.clone(), name.clone());
        }
    }
    let mut constraints = Vec::new();
    for item in advice {
        match &item.kind {
            AdviceKind::Index { index, .. } => add(index.schema.clone(), index.table.clone()),
            AdviceKind::ForeignKey { constraint } => constraints.push(constraint.clone()),
            _ => {}
        }
    }
    (tables, constraints)
}
