//! What a write costs: the rows each table got, whether updates were HOT
//! and why not, the index entries they wrote and the WAL.
//!
//! An update is HOT (a heap-only tuple) when the new version of the row
//! goes on the same page as the old one and no index refers to a column
//! that changed: then no index gets a new entry. Otherwise every index of
//! the table gets one, for every row.
//!
//! In connected mode, explainsql reads the transaction's own counters
//! (`pg_stat_xact_user_tables`) before and after the statement, inside the
//! transaction that is rolled back, with the indexes and the fillfactor of
//! each table written. This module reads that capture with the plan and
//! the statement's text, which tells the columns an `UPDATE` sets. It is
//! pure.

use serde::Serialize;

use crate::format;
use crate::ir::Plan;
use crate::locks::{Note, QualifiedName};
use crate::params::{block_comment, dollar_quote, quoted};
use crate::rules::Severity;

/// What explainsql read of the tables a statement wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteCapture {
    /// The tables the statement wrote rows to, its triggers' and cascades'
    /// included.
    pub tables: Vec<TableWrites>,
    pub server_version: u32,
}

/// The rows a table got from the statement, with its indexes.
#[derive(Debug, Clone, PartialEq)]
pub struct TableWrites {
    pub table: QualifiedName,
    pub inserted: i64,
    pub updated: i64,
    pub deleted: i64,
    /// Updates that were HOT.
    pub hot_updated: i64,
    /// Updates whose new version went to another page (PostgreSQL 16).
    pub newpage_updated: Option<i64>,
    /// The table's `fillfactor`, when it sets one.
    pub fillfactor: Option<u32>,
    pub indexes: Vec<IndexColumns>,
}

/// An index of a written table, and the columns it refers to.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexColumns {
    pub name: QualifiedName,
    /// Every column the index refers to: its keys, its `INCLUDE` columns,
    /// and those in its expressions and its predicate. A change to any of
    /// them stops an update from being HOT.
    pub columns: Vec<String>,
    /// BRIN, which summarizes ranges of pages rather than pointing to rows:
    /// from PostgreSQL 16, a change to its columns does not stop an update
    /// from being HOT.
    pub summarizing: bool,
    /// A partial index: rows outside its predicate get no entry.
    pub partial: bool,
    /// Primary key, unique or exclusion constraint.
    pub enforces: bool,
    /// A partition's index that belongs to an index of the partitioned
    /// table: it cannot be dropped alone.
    pub inherited: bool,
    /// Index scans since the statistics were reset.
    pub scans: Option<i64>,
}

/// What a write costs, and why its updates were not HOT.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Xray {
    /// `1 row updated in orders, not HOT: 1 index entry and 336 B of WAL
    /// per row.`
    pub summary: String,
    pub tables: Vec<TableXray>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal: Option<WalUse>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// The indexes whose columns the statement sets, that it would have to
    /// do without for its updates to be HOT, and that can be dropped:
    /// `--prove --allow-ddl` drops them in a transaction that is rolled
    /// back and runs the statement again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub to_drop: Vec<QualifiedName>,
    /// The statement run again without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<HotProof>,
}

/// The rows one table got.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableXray {
    /// `public` left out.
    pub table: String,
    pub inserted: i64,
    pub updated: i64,
    pub deleted: i64,
    pub hot_updated: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newpage_updated: Option<i64>,
    /// The index entries the rows wrote: one in each index for each row
    /// inserted and each update that was not HOT.
    pub index_entries: i64,
    pub indexes: usize,
    /// Some indexes are partial, so the entries are at most this many.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// The columns the statement sets that indexes refer to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocking: Vec<Blocking>,
}

/// A column the statement sets, and the indexes that refer to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocking {
    pub column: String,
    pub indexes: Vec<String>,
}

/// The WAL the statement wrote, from `EXPLAIN (ANALYZE, WAL)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WalUse {
    pub records: u64,
    /// Full-page images.
    pub fpi: u64,
    pub bytes: u64,
    /// Bytes for each row written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_row: Option<f64>,
}

/// The statement run again without the indexes that kept its updates from
/// being HOT, dropped in a transaction that was rolled back.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HotProof {
    pub dropped: Vec<String>,
    pub updated: i64,
    pub hot_updated: i64,
    pub index_entries: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_bytes: Option<u64>,
    /// `Without orders_status_idx: 1 of 1 update HOT, no index entries,
    /// 110 B of WAL per row.`
    pub summary: String,
}

impl WalUse {
    /// `WAL: 4 records, 1 full-page image, 2.0 kB.`
    pub fn describe(&self) -> String {
        let mut parts = vec![count(
            i64::try_from(self.records).unwrap_or(i64::MAX),
            "record",
            "records",
        )];
        if self.fpi > 0 {
            parts.push(count(
                i64::try_from(self.fpi).unwrap_or(i64::MAX),
                "full-page image",
                "full-page images",
            ));
        }
        #[allow(clippy::cast_precision_loss)]
        parts.push(size(self.bytes as f64));
        format!("WAL: {}.", parts.join(", "))
    }
}

impl TableXray {
    /// `1 updated (0 HOT, 1 to another page); 2 index entries`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.inserted > 0 {
            parts.push(format!("{} inserted", format::grouped(self.inserted)));
        }
        if self.updated > 0 {
            let mut part = format!(
                "{} updated ({} HOT",
                format::grouped(self.updated),
                format::grouped(self.hot_updated)
            );
            if let Some(newpage) = self.newpage_updated.filter(|&newpage| newpage > 0) {
                part.push_str(&format!(", {} to another page", format::grouped(newpage)));
            }
            part.push(')');
            parts.push(part);
        }
        if self.deleted > 0 {
            parts.push(format!("{} deleted", format::grouped(self.deleted)));
        }
        let entries = match (self.index_entries, self.indexes) {
            (_, 0) => "no index".to_owned(),
            (0, _) => "no index entries".to_owned(),
            (entries, indexes) => format!(
                "{}{} in {}",
                if self.partial { "up to " } else { "" },
                count(entries, "index entry", "index entries"),
                count(indexes as i64, "index", "indexes")
            ),
        };
        format!("{}; {entries}", parts.join(", "))
    }
}

/// What the statement's writes cost, from what was read of them, its plan
/// and its text. `None` when it wrote no row.
pub fn xray(capture: &WriteCapture, plan: &Plan, sql: &str) -> Option<Xray> {
    let assigned = assigned_columns(sql);
    let tables: Vec<(&TableWrites, TableXray)> = capture
        .tables
        .iter()
        .filter(|table| table.inserted + table.updated + table.deleted > 0)
        .map(|table| (table, table_xray(table, &assigned, capture.server_version)))
        .collect();
    if tables.is_empty() {
        return None;
    }
    let rows: i64 = tables
        .iter()
        .map(|(_, table)| table.inserted + table.updated + table.deleted)
        .sum();
    let wal = plan.root().wal.map(|wal| WalUse {
        records: wal.records,
        fpi: wal.fpi,
        bytes: wal.bytes,
        #[allow(clippy::cast_precision_loss)]
        per_row: (rows > 0).then(|| wal.bytes as f64 / rows as f64),
    });
    let mut notes = Vec::new();
    let mut to_drop = Vec::new();
    for (table, xray) in &tables {
        if let Some(note) = hot_note(table, xray, &assigned, &mut to_drop) {
            notes.push(note);
        }
        if let Some(note) = unused_note(table, xray) {
            notes.push(note);
        }
    }
    if let Some(note) = wal.and_then(fpi_note) {
        notes.push(note);
    }
    notes.sort_by_key(|note| std::cmp::Reverse(note.severity));
    let tables: Vec<TableXray> = tables.into_iter().map(|(_, xray)| xray).collect();
    Some(Xray {
        summary: summary(&tables, wal.as_ref(), rows),
        tables,
        wal,
        notes,
        to_drop,
        proof: None,
    })
}

/// Records the statement run again without [`Xray::to_drop`].
pub fn proven(xray: &mut Xray, capture: &WriteCapture, plan: &Plan) {
    let dropped: Vec<String> = xray.to_drop.iter().map(QualifiedName::show).collect();
    let tables: Vec<TableXray> = capture
        .tables
        .iter()
        .filter(|table| table.inserted + table.updated + table.deleted > 0)
        .map(|table| table_xray(table, &[], capture.server_version))
        .collect();
    let updated: i64 = tables.iter().map(|table| table.updated).sum();
    let hot_updated: i64 = tables.iter().map(|table| table.hot_updated).sum();
    let index_entries: i64 = tables.iter().map(|table| table.index_entries).sum();
    let rows: i64 = tables
        .iter()
        .map(|table| table.inserted + table.updated + table.deleted)
        .sum();
    let wal_bytes = plan.root().wal.map(|wal| wal.bytes);
    let mut summary = format!(
        "Without {} (dropped in a transaction that was rolled back): {} of {} HOT, {}",
        list(&dropped),
        format::grouped(hot_updated),
        count(updated, "update", "updates"),
        match index_entries {
            0 => "no index entries".to_owned(),
            entries => count(entries, "index entry", "index entries"),
        }
    );
    match wal_bytes {
        #[allow(clippy::cast_precision_loss)]
        Some(bytes) if rows > 0 => summary.push_str(&format!(
            ", {} of WAL per row",
            size(bytes as f64 / rows as f64)
        )),
        _ => {}
    }
    summary.push('.');
    if hot_updated < updated {
        summary.push_str(
            " Still not all HOT: the pages had no room for the new versions, or a column changed that another index refers to.",
        );
    }
    xray.proof = Some(HotProof {
        dropped,
        updated,
        hot_updated,
        index_entries,
        wal_bytes,
        summary,
    });
}

fn table_xray(table: &TableWrites, assigned: &[String], server_version: u32) -> TableXray {
    let not_hot = table.updated - table.hot_updated;
    let indexes = table.indexes.len();
    let blocking: Vec<Blocking> = if not_hot > 0 {
        assigned
            .iter()
            .filter_map(|column| {
                let indexes: Vec<String> = table
                    .indexes
                    .iter()
                    .filter(|index| blocks_hot(index, server_version))
                    .filter(|index| index.columns.contains(column))
                    .map(|index| index.name.show())
                    .collect();
                (!indexes.is_empty()).then(|| Blocking {
                    column: column.clone(),
                    indexes,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    TableXray {
        table: table.table.show(),
        inserted: table.inserted,
        updated: table.updated,
        deleted: table.deleted,
        hot_updated: table.hot_updated,
        newpage_updated: table.newpage_updated,
        index_entries: (table.inserted + not_hot) * indexes as i64,
        indexes,
        partial: table.indexes.iter().any(|index| index.partial),
        blocking,
    }
}

/// Whether a change to the index's columns stops an update from being HOT.
fn blocks_hot(index: &IndexColumns, server_version: u32) -> bool {
    !(index.summarizing && server_version >= 160_000)
}

/// Why updates were not HOT, and what to do.
fn hot_note(
    table: &TableWrites,
    xray: &TableXray,
    assigned: &[String],
    to_drop: &mut Vec<QualifiedName>,
) -> Option<Note> {
    let not_hot = xray.updated - xray.hot_updated;
    if not_hot == 0 {
        return None;
    }
    // `… were not HOT` reads after each.
    let updates = if xray.updated == 1 {
        format!("The update of {} was", xray.table)
    } else if not_hot == xray.updated {
        format!(
            "All {} updates of {} were",
            format::grouped(xray.updated),
            xray.table
        )
    } else {
        format!(
            "{} of the {} updates of {} {}",
            format::grouped(not_hot),
            format::grouped(xray.updated),
            xray.table,
            if not_hot == 1 { "was" } else { "were" }
        )
    };
    let entries = count(xray.indexes as i64, "index entry", "index entries");
    if !xray.blocking.is_empty() {
        let blocking: Vec<&IndexColumns> = table
            .indexes
            .iter()
            .filter(|index| {
                xray.blocking
                    .iter()
                    .any(|blocking| blocking.indexes.contains(&index.name.show()))
            })
            .collect();
        let droppable: Vec<&IndexColumns> = blocking
            .iter()
            .copied()
            .filter(|index| !index.enforces && !index.inherited)
            .collect();
        to_drop.extend(droppable.iter().map(|index| index.name.clone()));
        let unused: Vec<String> = droppable
            .iter()
            .filter(|index| index.scans == Some(0))
            .map(|index| index.name.show())
            .collect();
        let columns: Vec<String> = xray
            .blocking
            .iter()
            .map(|blocking| format!("{} ({})", blocking.column, list(&blocking.indexes)))
            .collect();
        let mut action = if unused.is_empty() {
            format!(
                "If no query needs {}, dropping {} lets such updates be HOT when the page has room; otherwise each one costs a new entry in every index.",
                if blocking.len() == 1 {
                    "that index"
                } else {
                    "those indexes"
                },
                if blocking.len() == 1 { "it" } else { "them" },
            )
        } else {
            format!(
                "{} {} not scanned since the statistics were reset: dropping {} lets such updates be HOT when the page has room. Check the statistics of every server that runs queries first.",
                list(&unused),
                if unused.len() == 1 { "is" } else { "are" },
                if unused.len() == 1 { "it" } else { "them" },
            )
        };
        if !droppable.is_empty() {
            action.push_str(&format!(
                " --prove --allow-ddl drops {} in a transaction that is rolled back and runs the statement again.",
                list(&droppable.iter().map(|index| index.name.show()).collect::<Vec<_>>())
            ));
        }
        return Some(Note {
            severity: if unused.is_empty() {
                Severity::Low
            } else {
                Severity::Medium
            },
            summary: format!(
                "{updates} not HOT: the statement sets {}, which an index refers to, so when the value changes, each such update writes a new entry in every index of the table: {entries} per row.",
                list(&columns),
            ),
            action: Some(action),
        });
    }
    if assigned.is_empty() {
        return Some(Note {
            severity: Severity::Low,
            summary: format!(
                "{updates} not HOT, and explainsql could not read which columns the statement sets."
            ),
            action: None,
        });
    }
    let fillfactor = table.fillfactor.unwrap_or(100);
    let newpage = match xray.newpage_updated {
        Some(newpage) if newpage > 0 => {
            format!(" ({} went to another page)", format::grouped(newpage))
        }
        _ => String::new(),
    };
    Some(Note {
        severity: Severity::Medium,
        summary: format!(
            "{updates} not HOT, though no index refers to the columns the statement sets: {}{newpage}, so each such update writes {entries}. The table's fillfactor is {fillfactor}{}.",
            if not_hot == 1 {
                "the page had no room for the new version"
            } else {
                "the pages had no room for the new versions"
            },
            if fillfactor == 100 {
                ", which leaves no room on its pages"
            } else {
                ""
            }
        ),
        action: Some(format!(
            "For a table updated often, ALTER TABLE {} SET (fillfactor = {}) leaves room on each page for updates. It applies to pages written from then on; VACUUM FULL or pg_repack rewrites the whole table with it.",
            xray.table,
            fillfactor.saturating_sub(10).max(50)
        )),
    })
}

/// Indexes nothing scans, that every insert and every update that is not
/// HOT writes to.
fn unused_note(table: &TableWrites, xray: &TableXray) -> Option<Note> {
    let writes = xray.inserted + xray.updated - xray.hot_updated;
    if writes == 0 {
        return None;
    }
    let unused: Vec<String> = table
        .indexes
        .iter()
        .filter(|index| index.scans == Some(0) && !index.enforces)
        // Already in the note on HOT updates.
        .filter(|index| {
            !xray
                .blocking
                .iter()
                .any(|blocking| blocking.indexes.contains(&index.name.show()))
        })
        .map(|index| index.name.show())
        .collect();
    if unused.is_empty() {
        return None;
    }
    Some(Note {
        severity: Severity::Low,
        summary: format!(
            "Each row inserted into {}, and each update that is not HOT, writes an entry in {}, which no scan used since the statistics were reset.",
            xray.table,
            list(&unused)
        ),
        action: Some(format!(
            "Drop {} if nothing needs {}, after checking the statistics of every server that runs queries.",
            if unused.len() == 1 { "it" } else { "them" },
            if unused.len() == 1 { "it" } else { "them" },
        )),
    })
}

fn fpi_note(wal: WalUse) -> Option<Note> {
    // A few among many records are not worth a note.
    if wal.fpi == 0 || wal.fpi * 10 < wal.records {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let bytes = wal.bytes as f64;
    let images = |n: u64| {
        if n == 1 {
            "is a full-page image"
        } else {
            "are full-page images"
        }
    };
    let records = if wal.fpi < wal.records {
        format!(
            "{} of the {} WAL records {}",
            format::grouped(i64::try_from(wal.fpi).unwrap_or(i64::MAX)),
            format::grouped(i64::try_from(wal.records).unwrap_or(i64::MAX)),
            images(wal.fpi)
        )
    } else if wal.records == 1 {
        "The WAL record is a full-page image".to_owned()
    } else {
        format!(
            "All {} WAL records are full-page images",
            format::grouped(i64::try_from(wal.records).unwrap_or(i64::MAX))
        )
    };
    Some(Note {
        severity: Severity::Low,
        summary: format!(
            "{records}: the first change to a page after a checkpoint writes the whole page to WAL, so most of the {} can be those images. Run again before the next checkpoint, the statement writes far less.",
            size(bytes)
        ),
        action: None,
    })
}

fn summary(tables: &[TableXray], wal: Option<&WalUse>, rows: i64) -> String {
    let mut parts = Vec::new();
    for (total, what) in [
        (
            tables.iter().map(|table| table.inserted).sum::<i64>(),
            "inserted",
        ),
        (tables.iter().map(|table| table.updated).sum(), "updated"),
        (tables.iter().map(|table| table.deleted).sum(), "deleted"),
    ] {
        if total > 0 {
            parts.push(format!("{} {what}", count(total, "row", "rows")));
        }
    }
    let mut text = parts.join(", ");
    let into = match parts.as_slice() {
        [only] if only.ends_with("inserted") => "into",
        [only] if only.ends_with("deleted") => "from",
        _ => "in",
    };
    match tables {
        [table] => text.push_str(&format!(" {into} {}", table.table)),
        _ => text.push_str(&format!(" {into} {} tables", tables.len())),
    }
    let updated: i64 = tables.iter().map(|table| table.updated).sum();
    let hot: i64 = tables.iter().map(|table| table.hot_updated).sum();
    if updated > 0 {
        text.push_str(&match hot {
            0 if updated == 1 => ", not HOT".to_owned(),
            0 => ", none of the updates HOT".to_owned(),
            hot if hot == updated => ", all HOT".to_owned(),
            hot => format!(", {} of the updates HOT", format::grouped(hot)),
        });
    }
    let entries: i64 = tables.iter().map(|table| table.index_entries).sum();
    #[allow(clippy::cast_precision_loss)]
    let per_row = |value: i64| value as f64 / rows.max(1) as f64;
    let mut costs = vec![match entries {
        0 => "no index entries".to_owned(),
        entries if entries == rows => "1 index entry".to_owned(),
        entries => format!("{} index entries", number(per_row(entries))),
    }];
    if let Some(bytes) = wal.and_then(|wal| wal.per_row) {
        costs.push(format!("{} of WAL", size(bytes)));
    }
    text.push_str(&format!(": {} per row.", costs.join(" and ")));
    // `1 row updated` reads better than `1 rows`.
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// The columns an `UPDATE` sets, as PostgreSQL names them (names without
/// quotes in lower case): in `UPDATE … SET`, `ON CONFLICT … DO UPDATE SET`
/// and `MERGE`'s `UPDATE SET`, also inside a `WITH`. Empty for statements
/// that set none.
pub fn assigned_columns(sql: &str) -> Vec<String> {
    let tokens = tokens(sql);
    let mut columns: Vec<String> = Vec::new();
    let mut depth = 0_i32;
    // A parenthesis is at the depth outside it.
    let depths: Vec<i32> = tokens
        .iter()
        .map(|token| match token {
            Token::Symbol('(') => {
                depth += 1;
                depth - 1
            }
            Token::Symbol(')') => {
                depth -= 1;
                depth
            }
            _ => depth,
        })
        .collect();
    for (at, token) in tokens.iter().enumerate() {
        if !token.is_keyword("set") || !after_update(&tokens, &depths, at) {
            continue;
        }
        let level = depths[at];
        let mut next = at + 1;
        // One assignment at a time: a target, `=`, an expression.
        while next < tokens.len() {
            match &tokens[next] {
                Token::Symbol('(') => {
                    // `(a, b) = (…)`: the first word of each element.
                    let mut first = true;
                    next += 1;
                    while next < tokens.len() && depths[next] > level {
                        match &tokens[next] {
                            Token::Word(name, _) if first => {
                                push(&mut columns, name);
                                first = false;
                            }
                            Token::Symbol(',') if depths[next] == level + 1 => first = true,
                            _ => {}
                        }
                        next += 1;
                    }
                }
                Token::Word(name, _) => {
                    push(&mut columns, name);
                    next += 1;
                }
                _ => break,
            }
            // To the end of the expression.
            let mut cases = 0;
            let mut more = false;
            while next < tokens.len() {
                let token = &tokens[next];
                if depths[next] < level {
                    break;
                }
                if depths[next] == level {
                    if token.is_keyword("case") {
                        cases += 1;
                    } else if token.is_keyword("end") {
                        cases -= 1;
                    } else if cases == 0
                        && (["from", "where", "returning", "when"]
                            .iter()
                            .any(|keyword| token.is_keyword(keyword))
                            || *token == Token::Symbol(';'))
                    {
                        break;
                    } else if cases == 0 && *token == Token::Symbol(',') {
                        more = true;
                        next += 1;
                        break;
                    }
                }
                next += 1;
            }
            if !more {
                break;
            }
        }
    }
    columns
}

fn push(columns: &mut Vec<String>, name: &str) {
    if !columns.iter().any(|column| column == name) {
        columns.push(name.to_owned());
    }
}

/// Whether the `SET` at `at` follows an `UPDATE` at the same depth: `UPDATE
/// ONLY s.t * AS a SET`, `DO UPDATE SET`, `THEN UPDATE SET`.
fn after_update(tokens: &[Token], depths: &[i32], at: usize) -> bool {
    tokens[..at]
        .iter()
        .zip(&depths[..at])
        .rev()
        .take(12)
        .take_while(|(token, depth)| {
            **depth == depths[at] && matches!(token, Token::Word(..) | Token::Symbol('.' | '*'))
        })
        .any(|(token, _)| token.is_keyword("update"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A name, in lower case unless quoted, and whether it was quoted.
    Word(String, bool),
    Symbol(char),
    /// A literal or a number.
    Value,
}

impl Token {
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self, Token::Word(word, false) if word == keyword)
    }
}

/// The statement's names, symbols and values, without comments.
fn tokens(sql: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = sql;
    while let Some(c) = rest.chars().next() {
        let (token, length) = if c.is_whitespace() {
            (None, c.len_utf8())
        } else if rest.starts_with("--") {
            (None, rest.find('\n').unwrap_or(rest.len()))
        } else if rest.starts_with("/*") {
            (None, block_comment(rest))
        } else if c == '\'' {
            (Some(Token::Value), quoted(rest, '\'', false))
        } else if c == '"' {
            let length = quoted(rest, '"', false);
            let name = rest[1..length.saturating_sub(1).max(1)].replace("\"\"", "\"");
            (Some(Token::Word(name, true)), length)
        } else if c == '$' {
            match dollar_quote(rest) {
                Some(length) => (Some(Token::Value), length),
                None => {
                    let length = 1 + rest[1..].bytes().take_while(u8::is_ascii_digit).count();
                    (Some(Token::Value), length)
                }
            }
        } else if c.is_alphabetic() || c == '_' {
            let length = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            let word = &rest[..length];
            // E'…' and the like: a literal with a prefix.
            if rest[length..].starts_with('\'') && word.len() == 1 {
                let escapes = word.eq_ignore_ascii_case("e");
                (
                    Some(Token::Value),
                    length + quoted(&rest[length..], '\'', escapes),
                )
            } else {
                (Some(Token::Word(word.to_lowercase(), false)), length)
            }
        } else if c.is_ascii_digit() {
            let length = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
                .unwrap_or(rest.len());
            (Some(Token::Value), length)
        } else {
            (Some(Token::Symbol(c)), c.len_utf8())
        };
        tokens.extend(token);
        rest = &rest[length.max(1).min(rest.len())..];
    }
    tokens
}

/// `336 B`, `1.9 kB`, `12.4 MB`.
fn size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{bytes:.0} B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1} kB", bytes / 1024.0)
    } else {
        format!("{:.1} MB", bytes / 1024.0 / 1024.0)
    }
}

/// `2`, `2.5`.
fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn count(n: i64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", format::grouped(n))
    }
}

/// `a`, `a and b`, `a, b and c`.
fn list<T: AsRef<str>>(items: &[T]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [init @ .., last] => format!(
            "{} and {}",
            init.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_columns_an_update_sets() {
        assert_eq!(
            assigned_columns(
                "UPDATE orders SET status = 'shipped', updated_at = now() WHERE id = $1"
            ),
            ["status", "updated_at"]
        );
        assert_eq!(
            assigned_columns(
                "update only public.orders * as o set (Status, \"Note\") = (select 'a', 'b') , amount = amount * 2 from customers c where c.id = o.customer_id returning *"
            ),
            ["status", "Note", "amount"]
        );
        // Names in literals and comments are not columns; a CASE's WHEN
        // does not end the list; arrays and fields.
        assert_eq!(
            assigned_columns(
                "UPDATE t SET -- note = 1\n a = CASE WHEN b > 0 THEN 'x, y = 2' ELSE $$z$$ END, c[1] = 2, d.f = 3 /* e = 4 */ WHERE true"
            ),
            ["a", "c", "d"]
        );
        // INSERT … ON CONFLICT, MERGE, and inside a WITH.
        assert_eq!(
            assigned_columns(
                "INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET n = t.n + excluded.n WHERE t.n < 10"
            ),
            ["n"]
        );
        assert_eq!(
            assigned_columns(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET n = s.n, m = 1 WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.n, 0)"
            ),
            ["n", "m"]
        );
        assert_eq!(
            assigned_columns(
                "WITH moved AS (UPDATE t SET n = 0 WHERE n < 0 RETURNING id) SELECT count(*) FROM moved"
            ),
            ["n"]
        );
        // No SET, or another SET.
        assert!(assigned_columns("DELETE FROM t WHERE id = 1").is_empty());
        assert!(assigned_columns("INSERT INTO t SELECT * FROM s").is_empty());
        assert!(assigned_columns("SELECT set FROM t").is_empty());
    }

    fn index(name: &str, columns: &[&str]) -> IndexColumns {
        IndexColumns {
            name: QualifiedName::new("public", name),
            columns: columns.iter().map(|column| (*column).to_owned()).collect(),
            summarizing: false,
            partial: false,
            enforces: false,
            inherited: false,
            scans: Some(100),
        }
    }

    fn orders(updated: i64, hot: i64, newpage: i64) -> WriteCapture {
        WriteCapture {
            tables: vec![TableWrites {
                table: QualifiedName::new("public", "orders"),
                inserted: 0,
                updated,
                deleted: 0,
                hot_updated: hot,
                newpage_updated: Some(newpage),
                fillfactor: None,
                indexes: vec![
                    IndexColumns {
                        enforces: true,
                        ..index("orders_pkey", &["id"])
                    },
                    IndexColumns {
                        scans: Some(0),
                        ..index("orders_updated_at_idx", &["updated_at"])
                    },
                    index("orders_status_idx", &["status", "created_at"]),
                    IndexColumns {
                        summarizing: true,
                        ..index("orders_created_brin", &["created_at", "note"])
                    },
                ],
            }],
            server_version: 160_004,
        }
    }

    fn update_plan(wal: &str) -> Plan {
        crate::parse(&format!(
            "Update on public.orders  (cost=0.42..8.44 rows=0 width=0) (actual time=0.093..0.093 rows=0 loops=1)\n  WAL: {wal}\n  ->  Index Scan using orders_pkey on public.orders  (cost=0.42..8.44 rows=1 width=10) (actual time=0.007..0.008 rows=1 loops=1)\n        Index Cond: (orders.id = 6)"
        ))
        .unwrap()
    }

    #[test]
    fn names_the_indexes_that_keep_updates_from_being_hot() {
        let sql =
            "UPDATE orders SET status = 'shipped', updated_at = now(), note = 'x' WHERE id = 6";
        let xray = xray(
            &orders(1, 0, 0),
            &update_plan("records=4 fpi=1 bytes=2000"),
            sql,
        )
        .unwrap();
        assert_eq!(
            xray.summary,
            "1 row updated in orders, not HOT: 4 index entries and 2.0 kB of WAL per row."
        );
        let table = &xray.tables[0];
        assert_eq!(table.index_entries, 4);
        // A BRIN index does not keep an update from being HOT from 16.
        assert_eq!(
            table.blocking,
            [
                Blocking {
                    column: "status".to_owned(),
                    indexes: vec!["orders_status_idx".to_owned()],
                },
                Blocking {
                    column: "updated_at".to_owned(),
                    indexes: vec!["orders_updated_at_idx".to_owned()],
                },
            ]
        );
        assert_eq!(
            table.describe(),
            "1 updated (0 HOT); 4 index entries in 4 indexes"
        );
        let hot = &xray.notes[0];
        assert_eq!(hot.severity, Severity::Medium);
        assert_eq!(
            hot.summary,
            "The update of orders was not HOT: the statement sets status (orders_status_idx) and updated_at (orders_updated_at_idx), which an index refers to, so when the value changes, each such update writes a new entry in every index of the table: 4 index entries per row."
        );
        assert!(
            hot.action.as_deref().unwrap().starts_with(
                "orders_updated_at_idx is not scanned since the statistics were reset"
            )
        );
        assert_eq!(
            xray.to_drop,
            [
                QualifiedName::new("public", "orders_updated_at_idx"),
                QualifiedName::new("public", "orders_status_idx"),
            ]
        );
        assert!(
            xray.notes[1]
                .summary
                .starts_with("1 of the 4 WAL records is a full-page image")
        );
        // Before 16, the BRIN index keeps it from being HOT too.
        let mut older = orders(1, 0, 0);
        older.server_version = 150_000;
        let xray = super::xray(&older, &update_plan("records=4 bytes=500"), sql).unwrap();
        assert_eq!(xray.tables[0].blocking.len(), 3);
    }

    #[test]
    fn says_when_the_page_had_no_room() {
        let sql = "UPDATE orders SET amount = amount + 1 WHERE id = 6";
        let xray = xray(&orders(2, 1, 1), &update_plan("records=3 bytes=400"), sql).unwrap();
        assert_eq!(
            xray.summary,
            "2 rows updated in orders, 1 of the updates HOT: 2 index entries and 200 B of WAL per row."
        );
        assert!(xray.to_drop.is_empty());
        let note = &xray.notes[0];
        assert_eq!(
            note.summary,
            "1 of the 2 updates of orders was not HOT, though no index refers to the columns the statement sets: the page had no room for the new version (1 went to another page), so each such update writes 4 index entries. The table's fillfactor is 100, which leaves no room on its pages."
        );
        assert!(
            note.action
                .as_deref()
                .unwrap()
                .contains("SET (fillfactor = 90)")
        );
        // The index nothing scans, written by each update that is not HOT.
        assert!(xray.notes[1].summary.contains("orders_updated_at_idx"));

        // All HOT: nothing to say about it.
        let xray = super::xray(&orders(1, 1, 0), &update_plan("records=1 bytes=110"), sql).unwrap();
        assert_eq!(
            xray.summary,
            "1 row updated in orders, all HOT: no index entries and 110 B of WAL per row."
        );
        assert!(xray.notes.is_empty());
        // Nothing written.
        assert!(super::xray(&orders(0, 0, 0), &update_plan("records=0 bytes=0"), sql).is_none());
    }

    #[test]
    fn words_the_rows_and_the_full_page_images() {
        let mut capture = orders(0, 0, 0);
        capture.tables[0].inserted = 10;
        let xray = xray(
            &capture,
            &update_plan("records=10 bytes=1000"),
            "INSERT INTO orders SELECT 1",
        )
        .unwrap();
        assert_eq!(
            xray.summary,
            "10 rows inserted into orders: 4 index entries and 100 B of WAL per row."
        );
        let mut capture = orders(0, 0, 0);
        capture.tables[0].deleted = 3;
        let xray = super::xray(
            &capture,
            &update_plan("records=3 bytes=1000"),
            "DELETE FROM orders",
        )
        .unwrap();
        assert_eq!(
            xray.summary,
            "3 rows deleted from orders: no index entries and 333 B of WAL per row."
        );

        let wal = |records, fpi| WalUse {
            records,
            fpi,
            bytes: 8000,
            per_row: None,
        };
        let summary = |records, fpi| fpi_note(wal(records, fpi)).map(|note| note.summary);
        assert!(
            summary(1, 1)
                .unwrap()
                .starts_with("The WAL record is a full-page image: ")
        );
        assert!(
            summary(3, 3)
                .unwrap()
                .starts_with("All 3 WAL records are full-page images: ")
        );
        assert!(
            summary(4, 2)
                .unwrap()
                .starts_with("2 of the 4 WAL records are full-page images: ")
        );
        // A few among many, or none.
        assert!(summary(100, 2).is_none());
        assert!(summary(4, 0).is_none());
    }

    #[test]
    fn records_the_proof() {
        let sql = "UPDATE orders SET updated_at = now() WHERE id = 6";
        let mut xray = xray(&orders(1, 0, 0), &update_plan("records=4 bytes=336"), sql).unwrap();
        assert_eq!(
            xray.to_drop,
            [QualifiedName::new("public", "orders_updated_at_idx")]
        );
        let mut after = orders(1, 1, 0);
        after.tables[0].indexes.remove(1);
        proven(&mut xray, &after, &update_plan("records=1 bytes=110"));
        let proof = xray.proof.unwrap();
        assert_eq!(
            proof.summary,
            "Without orders_updated_at_idx (dropped in a transaction that was rolled back): 1 of 1 update HOT, no index entries, 110 B of WAL per row."
        );
        assert_eq!(proof.index_entries, 0);
    }
}
