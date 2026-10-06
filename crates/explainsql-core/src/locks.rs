//! What a statement locks, and what that means for other sessions.
//!
//! In connected mode explainsql reads the locks its own backend holds once
//! the statement has been planned, or run, inside the transaction that is
//! rolled back. Locks last until the transaction ends, so `pg_locks` then
//! lists every lock the statement took: the parser's and the planner's,
//! which include every index of every table the planner looks at, and the
//! executor's. With a statement's parameters, the locks of one execution of
//! the cached generic plan, which locks every partition it may read, and of
//! a custom plan.
//!
//! This module is pure. It gets those locks ([`Capture`]), what the catalog
//! says about the locked relations, other sessions' locks on them and, when
//! a second connection watched the run, what the statement waited on. It
//! says what they mean ([`Footprint`]): how many relation locks the
//! statement takes and how many did not fit in the backend's fast-path
//! slots, which go to the shared lock table that sessions contend for
//! (`LWLock:LockManager`); which commands wait for it and then hold up every
//! later run; and whether a run's time includes waiting for another
//! session's lock.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Serialize, Serializer};

use crate::format;
use crate::ir::Plan;
use crate::rules::Severity;

/// A table-level lock mode, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockMode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

impl LockMode {
    pub const ALL: [LockMode; 8] = [
        LockMode::AccessShare,
        LockMode::RowShare,
        LockMode::RowExclusive,
        LockMode::ShareUpdateExclusive,
        LockMode::Share,
        LockMode::ShareRowExclusive,
        LockMode::Exclusive,
        LockMode::AccessExclusive,
    ];

    /// From `pg_locks.mode`: `AccessShareLock`.
    pub fn parse(mode: &str) -> Option<LockMode> {
        LockMode::ALL
            .into_iter()
            .find(|candidate| candidate.name() == mode)
    }

    /// As `pg_locks` names it: `AccessShareLock`.
    pub fn name(self) -> &'static str {
        match self {
            LockMode::AccessShare => "AccessShareLock",
            LockMode::RowShare => "RowShareLock",
            LockMode::RowExclusive => "RowExclusiveLock",
            LockMode::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
            LockMode::Share => "ShareLock",
            LockMode::ShareRowExclusive => "ShareRowExclusiveLock",
            LockMode::Exclusive => "ExclusiveLock",
            LockMode::AccessExclusive => "AccessExclusiveLock",
        }
    }

    /// Whether a lock in this mode and one in `other` on the same relation
    /// conflict: the table in the documentation's "Explicit Locking".
    pub fn conflicts_with(self, other: LockMode) -> bool {
        use LockMode::*;
        let conflicting: &[LockMode] = match self {
            AccessShare => &[AccessExclusive],
            RowShare => &[Exclusive, AccessExclusive],
            RowExclusive => &[Share, ShareRowExclusive, Exclusive, AccessExclusive],
            ShareUpdateExclusive => &[
                ShareUpdateExclusive,
                Share,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            Share => &[
                RowExclusive,
                ShareUpdateExclusive,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            ShareRowExclusive => &[
                RowExclusive,
                ShareUpdateExclusive,
                Share,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            Exclusive => &[
                RowShare,
                RowExclusive,
                ShareUpdateExclusive,
                Share,
                ShareRowExclusive,
                Exclusive,
                AccessExclusive,
            ],
            AccessExclusive => &LockMode::ALL,
        };
        conflicting.contains(&other)
    }

    /// Weak modes may take a fast-path slot: the modes of `SELECT`, `SELECT
    /// … FOR UPDATE` and data changes.
    pub fn weak(self) -> bool {
        self <= LockMode::RowExclusive
    }

    /// The schema changes and maintenance commands that take this mode,
    /// for the modes such commands take.
    fn commands(self) -> Option<&'static str> {
        match self {
            LockMode::ShareUpdateExclusive => {
                Some("VACUUM, ANALYZE, CREATE INDEX CONCURRENTLY, REINDEX CONCURRENTLY")
            }
            LockMode::Share => Some("CREATE INDEX without CONCURRENTLY"),
            LockMode::ShareRowExclusive => Some("CREATE TRIGGER, ALTER TABLE … ADD FOREIGN KEY"),
            LockMode::AccessExclusive => Some(
                "ALTER TABLE (most forms), DROP, TRUNCATE, REINDEX, CLUSTER, VACUUM FULL, LOCK TABLE",
            ),
            // EXCLUSIVE is taken by REFRESH MATERIALIZED VIEW CONCURRENTLY
            // only, on a materialized view.
            _ => None,
        }
    }
}

impl Serialize for LockMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

/// What a locked relation is, from `pg_class.relkind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Table,
    PartitionedTable,
    Index,
    PartitionedIndex,
    View,
    MaterializedView,
    Sequence,
    ForeignTable,
    Toast,
    Other,
}

impl RelationKind {
    /// From `pg_class.relkind`: `r`, `p`, `i`, `I`, …
    pub fn from_relkind(relkind: &str) -> RelationKind {
        match relkind {
            "r" => RelationKind::Table,
            "p" => RelationKind::PartitionedTable,
            "i" => RelationKind::Index,
            "I" => RelationKind::PartitionedIndex,
            "v" => RelationKind::View,
            "m" => RelationKind::MaterializedView,
            "S" => RelationKind::Sequence,
            "f" => RelationKind::ForeignTable,
            "t" => RelationKind::Toast,
            _ => RelationKind::Other,
        }
    }

    fn is_index(self) -> bool {
        matches!(self, RelationKind::Index | RelationKind::PartitionedIndex)
    }
}

/// A lock the statement's backend holds, as `pg_locks` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    /// `relation`, `transactionid`, `tuple`, `advisory`, `object`, …
    pub locktype: String,
    /// The locked relation, for relation, page and tuple locks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<u32>,
    /// `AccessShareLock`, …
    pub mode: String,
    pub granted: bool,
    /// Taken through one of the backend's fast-path slots, rather than in
    /// the shared lock table.
    pub fastpath: bool,
}

/// What the catalog says about a locked relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Relation {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// The table the relation belongs to: itself for a table, the indexed
    /// table for an index, the main table for a TOAST table or its index.
    pub table: QualifiedName,
    /// The partitioned table at the top of the table's partition tree,
    /// when the table is a partition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<QualifiedName>,
    /// An index: how many index scans used it since the statistics were
    /// last reset (`pg_stat_all_indexes.idx_scan`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scans: Option<i64>,
    /// An index that enforces a constraint: a primary key, a unique or an
    /// exclusion constraint. It may be needed even if no scan uses it.
    pub enforces: bool,
}

/// A relation's schema and name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct QualifiedName {
    pub schema: String,
    pub name: String,
}

impl QualifiedName {
    pub fn new(schema: &str, name: &str) -> Self {
        QualifiedName {
            schema: schema.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The name as plans show it: without the `public` schema.
    pub fn show(&self) -> String {
        if self.schema == "public" || self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

/// Another session's lock on a relation the statement locks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Other {
    /// `None` for a prepared transaction (`PREPARE TRANSACTION`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    pub relation: u32,
    pub mode: String,
    pub granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// `active`, `idle in transaction`, …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The session's current or last statement, as `pg_stat_activity`
    /// shows it to explainsql's role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// For how long, in seconds: waiting for the lock (PostgreSQL 14), or
    /// in its transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

/// What the statement's processes were doing while it ran, sampled from
/// `pg_stat_activity` by a second connection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Waits {
    /// Samples taken, each counting the leader and every parallel worker.
    pub samples: u32,
    /// Milliseconds between samples, on average.
    pub interval_ms: f64,
    /// By what the process was doing: `CPU` when it waited on nothing, else
    /// the wait event, as `IO:DataFileRead`. Most first.
    pub events: Vec<WaitCount>,
    /// Sessions holding a lock the statement waited for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitCount {
    pub event: String,
    pub samples: u32,
}

/// A session that held a lock the statement waited for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    pub pid: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl Waits {
    /// Samples of a process waiting for a heavyweight lock: another
    /// session's, as explainsql's own transaction holds nothing else.
    pub fn lock_samples(&self) -> u32 {
        self.events
            .iter()
            .filter(|count| count.event.starts_with("Lock:"))
            .map(|count| count.samples)
            .sum()
    }

    /// `While it ran (240 samples, every 10 ms): 82% CPU, 18%
    /// IO:DataFileRead.`; none when the run was too short for a sample.
    pub fn line(&self) -> Option<String> {
        (self.samples > 0).then(|| {
            format!(
                "While it ran ({} {}, every {:.0} ms): {}.",
                format::grouped(i64::from(self.samples)),
                if self.samples == 1 {
                    "sample"
                } else {
                    "samples"
                },
                self.interval_ms,
                self.profile()
            )
        })
    }

    /// About how long the statement waited for locks: `300 ms`, `1.20 s`.
    /// Sampled, so no finer than the interval.
    pub fn waited(&self) -> String {
        let ms = f64::from(self.lock_samples()) * self.interval_ms;
        if ms < 1000.0 {
            format!("{ms:.0} ms")
        } else {
            format::duration(ms)
        }
    }

    /// Counts samples by event, most first, then by name.
    pub fn new(samples: u32, interval_ms: f64, counts: BTreeMap<String, u32>) -> Waits {
        let mut events: Vec<WaitCount> = counts
            .into_iter()
            .map(|(event, samples)| WaitCount { event, samples })
            .collect();
        events.sort_by(|a, b| b.samples.cmp(&a.samples).then(a.event.cmp(&b.event)));
        Waits {
            samples,
            interval_ms,
            events,
            blockers: Vec::new(),
        }
    }

    /// `82% CPU, 12% IO:DataFileRead, 6% Lock:relation`.
    pub fn profile(&self) -> String {
        let total: u32 = self.events.iter().map(|count| count.samples).sum();
        if total == 0 {
            return String::new();
        }
        self.events
            .iter()
            .map(|count| {
                format!(
                    "{} {}",
                    format::percent(f64::from(count.samples) / f64::from(total)),
                    count.event
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// When the locks were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// After `EXPLAIN` without `ANALYZE`: what parsing and planning lock.
    Planned,
    /// After `EXPLAIN ANALYZE`: planning and running.
    Ran,
    /// One execution of the cached generic plan of the prepared statement,
    /// as an application gets it once PostgreSQL switches to that plan.
    Generic,
    /// One execution with a custom plan, made for the values given.
    Custom,
}

impl Stage {
    /// `Locks after running it`, for a heading.
    pub fn heading(self) -> &'static str {
        match self {
            Stage::Planned => "Locks the statement takes to be planned",
            Stage::Ran => "Locks the statement takes",
            Stage::Generic => "Locks of each execution of the generic plan",
            Stage::Custom => "Locks of each execution of a custom plan",
        }
    }
}

/// What explainsql read in connected mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub stage: Stage,
    /// The locks the statement added to those its transaction held before.
    pub held: Vec<Held>,
    pub relations: Vec<Relation>,
    pub others: Vec<Other>,
    /// The fast-path slots of a backend: see [`fast_path_slots`].
    pub fast_path_slots: u32,
    /// `max_locks_per_transaction`.
    pub max_locks_per_transaction: Option<u32>,
    pub server_version: u32,
    /// When the statistics were last reset (`pg_stat_database`): index
    /// scans count from then.
    pub stats_reset: Option<String>,
    /// What the statement waited on while it ran, when watched.
    pub waits: Option<Waits>,
}

/// Introduces [`Footprint::blocked_by`].
pub const BLOCKED_BY: &str = "These commands would wait for the statement's locks, and while they wait, every later run of the statement waits behind them:";

/// What to do about [`Footprint::blocked_by`].
pub const LOCK_TIMEOUT: &str = "In schema changes, SET lock_timeout = '2s' (or so) makes a command that has to wait give up instead of holding up every query on the table; CREATE INDEX CONCURRENTLY and REINDEX CONCURRENTLY take weaker locks.";

/// How many relation locks a backend can take through its fast path, in
/// slots of its own, before it has to use the shared lock table: 16 before
/// PostgreSQL 18. From 18, `max_locks_per_transaction` rounded up to a
/// power of two, in groups of 16 (from 16 to 16,384), each relation
/// hashed to one group.
pub fn fast_path_slots(server_version: u32, max_locks_per_transaction: Option<u32>) -> u32 {
    const PER_GROUP: u32 = 16;
    const MAX_GROUPS: u32 = 1024;
    if server_version < 180_000 {
        return PER_GROUP;
    }
    let locks = max_locks_per_transaction.unwrap_or(64).max(1);
    let groups = (locks.next_power_of_two() / PER_GROUP).clamp(1, MAX_GROUPS);
    groups * PER_GROUP
}

/// What the statement's locks mean.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Footprint {
    pub stage: Stage,
    /// In one sentence: `27 relation locks on 1 table, 11 of them outside
    /// the fast path.`
    pub summary: String,
    /// Locks on relations: tables, partitions, indexes, views, sequences.
    pub relation_locks: usize,
    /// Relation locks in a weak mode that are in the shared lock table
    /// rather than in a fast-path slot.
    pub outside_fast_path: usize,
    pub fast_path_slots: u32,
    /// By table (a partitioned table with its partitions), most locks
    /// first.
    pub tables: Vec<TableLocks>,
    /// Locks on something else than a relation: `its transaction ID
    /// (ExclusiveLock)`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<String>,
    /// The commands that wait for the statement and then hold up every
    /// later run of it, by table: `ALTER TABLE (most forms), … on orders`.
    pub blocked_by: Vec<String>,
    /// Most severe first.
    pub notes: Vec<Note>,
    /// Other sessions whose locks conflict with the statement's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Other>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waits: Option<Waits>,
}

/// The locks on one table, its partitions and their indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableLocks {
    /// The table, or the partitioned table its partitions belong to.
    pub table: String,
    /// The strongest mode on the table or its partitions (or on anything
    /// in the group, without them).
    pub mode: LockMode,
    pub locks: usize,
    /// Partitions locked.
    pub partitions: usize,
    /// Partitions the plan has, when partitions are locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partitions_in_plan: Option<usize>,
    /// Indexes locked, of the table or its partitions.
    pub indexes: usize,
    /// Indexes locked that the plan uses.
    pub indexes_in_plan: usize,
    /// Indexes locked that the plan does not use, that no scan used since
    /// the statistics were reset, and that enforce no constraint.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub never_scanned: Vec<String>,
    pub outside_fast_path: usize,
}

impl TableLocks {
    /// `AccessShareLock; 12 partitions (the plan has 1), 13 indexes (it
    /// uses 1)`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.partitions > 0 {
            let mut part = count(self.partitions, "partition", "partitions");
            match self.partitions_in_plan {
                Some(0) => part.push_str(" (the plan has none)"),
                Some(read) if read < self.partitions => {
                    part.push_str(&format!(" (the plan has {read})"));
                }
                _ => {}
            }
            parts.push(part);
        }
        if self.indexes > 0 {
            let used = match self.indexes_in_plan {
                0 => " (the plan uses none)".to_owned(),
                used if used == self.indexes => String::new(),
                used => format!(" (the plan uses {used})"),
            };
            parts.push(format!("{}{used}", count(self.indexes, "index", "indexes")));
        }
        let mut text = format!(
            "{}: {}",
            count(self.locks, "lock", "locks"),
            self.mode.name()
        );
        if !parts.is_empty() {
            text.push_str(&format!(" on the table, {}", parts.join(", ")));
        }
        if self.outside_fast_path > 0 {
            text.push_str(&format!(
                "; {} outside the fast path",
                self.outside_fast_path
            ));
        }
        text
    }
}

/// Something the locks call for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub severity: Severity,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// What the locks of a capture mean, for the statement's plan in that
/// capture.
pub fn footprint(capture: &Capture, plan: &Plan) -> Footprint {
    let relations: BTreeMap<u32, &Relation> = capture
        .relations
        .iter()
        .map(|relation| (relation.oid, relation))
        .collect();
    let held: Vec<&Held> = capture.held.iter().filter(|lock| lock.granted).collect();
    let relation_locks: Vec<(&Held, Option<&Relation>)> = held
        .iter()
        .filter(|lock| lock.locktype == "relation")
        .map(|lock| {
            (
                *lock,
                lock.relation.and_then(|oid| relations.get(&oid).copied()),
            )
        })
        .collect();
    let outside =
        |lock: &Held| !lock.fastpath && LockMode::parse(&lock.mode).is_some_and(LockMode::weak);
    let outside_fast_path = relation_locks
        .iter()
        .filter(|(lock, _)| outside(lock))
        .count();
    let fast = relation_locks
        .iter()
        .filter(|(lock, _)| lock.fastpath)
        .count();

    // What the plan reads and uses.
    let used_indexes: BTreeSet<&str> = plan
        .nodes
        .iter()
        .filter_map(|node| node.index_name.as_deref())
        .collect();
    let scanned: BTreeSet<(Option<&str>, &str)> = plan
        .nodes
        .iter()
        .filter_map(|node| Some((node.schema.as_deref(), node.relation_name.as_deref()?)))
        .collect();
    let in_plan = |relation: &Relation| {
        scanned.contains(&(Some(relation.schema.as_str()), relation.name.as_str()))
            || scanned.contains(&(None, relation.name.as_str()))
    };

    // By table: a partitioned table with its partitions and their indexes.
    let mut groups: BTreeMap<QualifiedName, Vec<(&Held, Option<&Relation>)>> = BTreeMap::new();
    for &(lock, relation) in &relation_locks {
        let key = match relation {
            Some(relation) => relation
                .root
                .clone()
                .unwrap_or_else(|| relation.table.clone()),
            None => QualifiedName::new(
                "",
                &format!("relation {}", lock.relation.unwrap_or_default()),
            ),
        };
        groups.entry(key).or_default().push((lock, relation));
    }
    let mut tables: Vec<TableLocks> = groups
        .iter()
        .map(|(name, locks)| {
            let modes = |table_level: bool| {
                locks
                    .iter()
                    .filter(|(_, relation)| {
                        !table_level || relation.is_some_and(|relation| !relation.kind.is_index())
                    })
                    .filter_map(|(lock, _)| LockMode::parse(&lock.mode))
                    .max()
            };
            let mode = modes(true)
                .or_else(|| modes(false))
                .unwrap_or(LockMode::AccessShare);
            let partitions: BTreeSet<u32> = locks
                .iter()
                .filter_map(|(_, relation)| *relation)
                .filter(|relation| relation.root.is_some() && !relation.kind.is_index())
                .map(|relation| relation.oid)
                .collect();
            let partitions_in_plan = (!partitions.is_empty()).then(|| {
                partitions
                    .iter()
                    .filter(|oid| relations.get(oid).is_some_and(|relation| in_plan(relation)))
                    .count()
            });
            let indexes: BTreeMap<u32, &Relation> = locks
                .iter()
                .filter_map(|(_, relation)| *relation)
                .filter(|relation| relation.kind.is_index())
                .map(|relation| (relation.oid, relation))
                .collect();
            let indexes_in_plan = indexes
                .values()
                .filter(|index| used_indexes.contains(index.name.as_str()))
                .count();
            let never_scanned = indexes
                .values()
                .filter(|index| {
                    index.kind == RelationKind::Index
                        && index.scans == Some(0)
                        && !index.enforces
                        && !used_indexes.contains(index.name.as_str())
                })
                .map(|index| QualifiedName::new(&index.schema, &index.name).show())
                .collect();
            TableLocks {
                table: name.show(),
                mode,
                locks: locks.len(),
                partitions: partitions.len(),
                partitions_in_plan,
                indexes: indexes.len(),
                indexes_in_plan,
                never_scanned,
                outside_fast_path: locks.iter().filter(|(lock, _)| outside(lock)).count(),
            }
        })
        .collect();
    tables.sort_by(|a, b| b.locks.cmp(&a.locks).then(a.table.cmp(&b.table)));

    let other = held
        .iter()
        .filter(|lock| !matches!(lock.locktype.as_str(), "relation" | "virtualxid"))
        .map(|lock| describe_other(lock))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut notes = Vec::new();
    if outside_fast_path > 0 {
        notes.push(fast_path_note(capture, &tables, outside_fast_path, fast));
    }
    for table in &tables {
        if let Some(note) = partitions_note(capture.stage, table) {
            notes.push(note);
        }
    }
    let never_scanned: Vec<&String> = tables
        .iter()
        .flat_map(|table| &table.never_scanned)
        .collect();
    if outside_fast_path == 0 && !never_scanned.is_empty() {
        notes.push(Note {
            severity: Severity::Low,
            summary: format!(
                "The planner locks every index of the tables it plans, used or not: {} {} not used by this plan and not scanned since {}.",
                list(&never_scanned),
                if never_scanned.len() == 1 { "is" } else { "are" },
                since(capture)
            ),
            action: Some(
                "An index nothing uses adds a lock to every run of every statement on its table, and work to every write: drop it, after checking the statistics of every server that runs queries (replicas count their own scans).".to_owned(),
            ),
        });
    }
    let conflicts = conflicts(capture, &relation_locks);
    for other in &conflicts {
        notes.push(conflict_note(other, &relations));
    }
    if let Some(waits) = &capture.waits {
        if let Some(note) = waits_note(waits) {
            notes.push(note);
        }
    }
    notes.sort_by_key(|note| std::cmp::Reverse(note.severity));

    let summary = summary(
        capture.stage,
        relation_locks.len(),
        tables.len(),
        outside_fast_path,
        capture.fast_path_slots,
    );
    Footprint {
        stage: capture.stage,
        summary,
        relation_locks: relation_locks.len(),
        outside_fast_path,
        fast_path_slots: capture.fast_path_slots,
        blocked_by: blocked_by(&tables, &relation_locks),
        tables,
        other,
        notes,
        conflicts,
        waits: capture.waits.clone(),
    }
}

/// Adds what the generic plan's locks cost against a custom plan's.
pub fn compare_executions(footprints: &mut [Footprint]) {
    let generic = footprints
        .iter()
        .position(|footprint| footprint.stage == Stage::Generic);
    let custom = footprints
        .iter()
        .find(|footprint| footprint.stage == Stage::Custom)
        .map(|footprint| footprint.relation_locks);
    let (Some(generic), Some(custom)) = (generic, custom) else {
        return;
    };
    let footprint = &mut footprints[generic];
    if footprint.relation_locks <= custom {
        return;
    }
    footprint.notes.push(Note {
        severity: if footprint.outside_fast_path > 0 {
            Severity::Medium
        } else {
            Severity::Low
        },
        summary: format!(
            "Each execution of the generic plan takes {} relation locks; a custom plan for the same values takes {custom}. PostgreSQL may switch to the generic plan after five executions, and then pays this on every one.",
            footprint.relation_locks
        ),
        action: Some(
            "For this statement, plan_cache_mode = force_custom_plan (pgJDBC: prepareThreshold=0) locks only what each execution's values need, at the cost of planning each time.".to_owned(),
        ),
    });
    footprint
        .notes
        .sort_by_key(|note| std::cmp::Reverse(note.severity));
}

fn summary(stage: Stage, locks: usize, tables: usize, outside: usize, slots: u32) -> String {
    let mut text = match stage {
        Stage::Planned | Stage::Ran => String::new(),
        Stage::Generic => "Each execution of the generic plan: ".to_owned(),
        Stage::Custom => "Each execution of a custom plan: ".to_owned(),
    };
    text.push_str(&format!(
        "{} on {}",
        count(locks, "relation lock", "relation locks"),
        count(tables, "table", "tables")
    ));
    if outside > 0 {
        text.push_str(&format!(
            ", {outside} of them outside the fast path ({slots} slots)"
        ));
    }
    text.push('.');
    capitalized(&text)
}

fn fast_path_note(capture: &Capture, tables: &[TableLocks], outside: usize, fast: usize) -> Note {
    let slots = capture.fast_path_slots;
    let (severity, why) = if capture.server_version >= 180_000 {
        (
            Severity::Medium,
            format!(
                "did not get a fast-path slot: the backend has {slots}, in groups of 16 that each take a share of the relations"
            ),
        )
    } else if fast >= slots as usize {
        (
            Severity::Medium,
            format!("did not fit in the backend's {slots} fast-path slots"),
        )
    } else {
        // Not an overflow: a strong lock moved them there.
        let total: usize = tables.iter().map(|table| table.locks).sum();
        return Note {
            severity: Severity::Low,
            summary: format!(
                "{outside} of the {total} relation locks {} in the shared lock table rather than a fast-path slot: another session asked for a strong lock on {} now or a moment ago, which moves every fast-path lock on {} there.",
                if outside == 1 { "is" } else { "are" },
                if outside == 1 {
                    "its relation"
                } else {
                    "their relations"
                },
                if outside == 1 { "it" } else { "them" },
            ),
            action: None,
        };
    };
    let total: usize = tables.iter().map(|table| table.locks).sum();
    let mut actions = Vec::new();
    let unused: Vec<&String> = tables
        .iter()
        .flat_map(|table| &table.never_scanned)
        .collect();
    if !unused.is_empty() {
        actions.push(format!(
            "drop the indexes nothing uses ({}: not scanned since {})",
            list(&unused),
            since(capture)
        ));
    }
    if tables.iter().any(|table| {
        table
            .partitions_in_plan
            .is_some_and(|read| read < table.partitions)
    }) {
        actions.push("let the planner rule out partitions when planning (below)".to_owned());
    }
    let mut action = if actions.is_empty() {
        String::new()
    } else {
        format!("To take fewer locks, {}. ", actions.join("; "))
    };
    if capture.server_version >= 180_000 {
        action.push_str(&match capture.max_locks_per_transaction {
            Some(locks) => format!(
                "A higher max_locks_per_transaction (now {locks}; it takes a restart) gives each backend more fast-path slots."
            ),
            None => "A higher max_locks_per_transaction (it takes a restart) gives each backend more fast-path slots.".to_owned(),
        });
    } else {
        action.push_str(
            "From PostgreSQL 18, max_locks_per_transaction sizes the fast path: 64 slots by default.",
        );
    }
    Note {
        severity,
        summary: format!(
            "{outside} of the {total} relation locks {why}. Many sessions running statements like this at once contend for the shared lock table (wait event LWLock:LockManager), which slows them all down."
        ),
        action: Some(action),
    }
}

fn partitions_note(stage: Stage, table: &TableLocks) -> Option<Note> {
    let in_plan = table.partitions_in_plan?;
    if in_plan >= table.partitions {
        return None;
    }
    let severity = if table.outside_fast_path > 0 {
        Severity::Medium
    } else {
        Severity::Low
    };
    let indexes = if table.indexes > 0 {
        format!(" and {}", count(table.indexes, "index", "indexes"))
    } else {
        String::new()
    };
    let in_plan = match in_plan {
        0 => "none".to_owned(),
        read => read.to_string(),
    };
    match stage {
        Stage::Generic => Some(Note {
            severity,
            summary: format!(
                "Each execution of the generic plan locks {} of {}{indexes}, whatever the values; the plan has {in_plan} of them once run-time pruning drops the rest.",
                count(table.partitions, "partition", "partitions"),
                table.table
            ),
            action: Some(
                "The number of locks grows with the partitions. For this statement, plan_cache_mode = force_custom_plan (pgJDBC: prepareThreshold=0) plans each execution for its values and locks only the partitions they need.".to_owned(),
            ),
        }),
        Stage::Planned | Stage::Ran | Stage::Custom => Some(Note {
            severity,
            summary: format!(
                "The statement locks {} of {}{indexes}, but the plan has {in_plan} of them: the planner could not rule the others out when planning, and run-time pruning dropped them after they were locked.",
                count(table.partitions, "partition", "partitions"),
                table.table
            ),
            action: Some(
                "Compare the partition key with a constant, or a parameter of a custom plan, rather than an expression the planner cannot evaluate, such as one of now(): the planner then prunes the partitions and locks only those it reads.".to_owned(),
            ),
        }),
    }
}

/// The commands that would wait for the statement, by table: those whose
/// lock conflicts with the statement's strongest lock on the table, and
/// for indexes the plan does not use, the commands that lock an index.
fn blocked_by(tables: &[TableLocks], relation_locks: &[(&Held, Option<&Relation>)]) -> Vec<String> {
    let mut by_commands: BTreeMap<Vec<&'static str>, Vec<&str>> = BTreeMap::new();
    for table in tables {
        let commands: Vec<&'static str> = LockMode::ALL
            .into_iter()
            .rev()
            .filter(|mode| mode.conflicts_with(table.mode))
            .filter_map(LockMode::commands)
            .collect();
        if !commands.is_empty() {
            by_commands.entry(commands).or_default().push(&table.table);
        }
    }
    let mut lines: Vec<String> = by_commands
        .into_iter()
        .map(|(commands, tables)| format!("{} on {}", commands.join("; "), list(&tables)))
        .collect();
    let unused: usize = tables
        .iter()
        .map(|table| table.indexes - table.indexes_in_plan)
        .sum();
    let indexes = relation_locks
        .iter()
        .filter(|(_, relation)| relation.is_some_and(|relation| relation.kind.is_index()))
        .count();
    let which = match (indexes, unused) {
        (_, 0) => None,
        (1, _) => Some("the index it locks, which the plan does not use".to_owned()),
        (indexes, unused) if unused >= indexes => Some(format!(
            "any of the {indexes} indexes it locks, none of which the plan uses"
        )),
        (indexes, unused) => Some(format!(
            "any of the {indexes} indexes it locks, {unused} of which the plan does not use"
        )),
    };
    if let Some(which) = which {
        lines.push(format!(
            "REINDEX, DROP INDEX or ALTER INDEX of {which}: the planner locks every index of a table it plans"
        ));
    }
    lines
}

fn describe_other(lock: &Held) -> String {
    match lock.locktype.as_str() {
        "transactionid" => format!(
            "its transaction ID ({}): it writes, so a session that changes or locks the same rows waits for its transaction to end",
            lock.mode
        ),
        "tuple" => format!("a row ({})", lock.mode),
        "advisory" => format!("an advisory lock ({})", lock.mode),
        "page" => format!("a page ({})", lock.mode),
        "extend" => format!("extending a relation ({})", lock.mode),
        other => format!("{other} ({})", lock.mode),
    }
}

/// Other sessions' locks that conflict with the statement's on the same
/// relation.
fn conflicts(capture: &Capture, relation_locks: &[(&Held, Option<&Relation>)]) -> Vec<Other> {
    let ours: BTreeMap<u32, LockMode> = relation_locks
        .iter()
        .filter_map(|(lock, _)| Some((lock.relation?, LockMode::parse(&lock.mode)?)))
        .fold(BTreeMap::new(), |mut modes, (oid, mode)| {
            let entry = modes.entry(oid).or_insert(mode);
            *entry = (*entry).max(mode);
            modes
        });
    capture
        .others
        .iter()
        .filter(|other| {
            let (Some(ours), Some(theirs)) =
                (ours.get(&other.relation), LockMode::parse(&other.mode))
            else {
                return false;
            };
            ours.conflicts_with(theirs)
        })
        .cloned()
        .collect()
}

fn conflict_note(other: &Other, relations: &BTreeMap<u32, &Relation>) -> Note {
    let relation = relations
        .get(&other.relation)
        .map(|relation| QualifiedName::new(&relation.schema, &relation.name).show())
        .unwrap_or_else(|| format!("relation {}", other.relation));
    let who = capitalized(&session(
        other.pid,
        other.application.as_deref(),
        other.state.as_deref(),
        other.query.as_deref(),
    ));
    let how_long = other
        .seconds
        .map(|seconds| format!(" for {}", format::duration(seconds * 1000.0)))
        .unwrap_or_default();
    if other.granted {
        Note {
            severity: Severity::High,
            summary: format!(
                "{who} has held {} on {relation}{how_long}: the statement waits for it.",
                other.mode
            ),
            action: Some(
                "Its time then includes the wait. Find out why that session keeps the lock; an idle transaction holding it can be ended with pg_terminate_backend.".to_owned(),
            ),
        }
    } else {
        Note {
            severity: Severity::High,
            summary: format!(
                "{who} has waited{how_long} for {} on {relation}, behind sessions such as this one: until it gets the lock, every new run of the statement waits behind it.",
                other.mode
            ),
            action: Some(
                "Give schema changes a lock_timeout (SET lock_timeout = '2s') so that one that has to wait gives up instead of holding up every new query on the table.".to_owned(),
            ),
        }
    }
}

fn waits_note(waits: &Waits) -> Option<Note> {
    let locked = waits.lock_samples();
    if locked == 0 {
        return None;
    }
    let total: u32 = waits.events.iter().map(|count| count.samples).sum();
    let blockers: Vec<String> = waits
        .blockers
        .iter()
        .map(|blocker| {
            session(
                Some(blocker.pid),
                blocker.application.as_deref(),
                blocker.state.as_deref(),
                blocker.query.as_deref(),
            )
        })
        .collect();
    let held_by = if blockers.is_empty() {
        String::new()
    } else {
        format!(" Held by {}.", blockers.join("; "))
    };
    Some(Note {
        severity: Severity::High,
        summary: format!(
            "While it ran, the statement waited for another session's lock in {} of the samples (about {}): its time includes that wait, which is not the plan's.{held_by}",
            format::percent(f64::from(locked) / f64::from(total.max(1))),
            waits.waited()
        ),
        action: Some(
            "Run it again once that session is done to measure the plan alone.".to_owned(),
        ),
    })
}

/// `session 4521 (psql, idle in transaction: UPDATE orders …)`.
fn session(
    pid: Option<i32>,
    application: Option<&str>,
    state: Option<&str>,
    query: Option<&str>,
) -> String {
    let Some(pid) = pid else {
        return "a prepared transaction".to_owned();
    };
    let mut details = [application, state]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(query) = query.filter(|query| !query.trim().is_empty()) {
        if !details.is_empty() {
            details.push_str(": ");
        }
        details.push_str(&shorten(query, 80));
    }
    if details.is_empty() {
        format!("session {pid}")
    } else {
        format!("session {pid} ({details})")
    }
}

/// The text with its first letter in upper case.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn since(capture: &Capture) -> String {
    match &capture.stats_reset {
        Some(reset) => reset.split(['.', '+']).next().unwrap_or(reset).to_owned(),
        None => "the statistics began".to_owned(),
    }
}

fn shorten(text: &str, width: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= width {
        text
    } else {
        let cut: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    }
}

/// `1 index`, `3 indexes`.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", format::grouped(n as i64))
    }
}

/// `a`, `a and b`, `a, b and c`; past five, `a, b, c, d, e and 3 more`.
fn list<T: AsRef<str>>(items: &[T]) -> String {
    const SHOWN: usize = 5;
    let names: Vec<&str> = items.iter().map(AsRef::as_ref).collect();
    match names.len() {
        0 => String::new(),
        1 => names[0].to_owned(),
        n if n <= SHOWN => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
        n => format!("{} and {} more", names[..SHOWN].join(", "), n - SHOWN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation(oid: u32, name: &str, relkind: &str, table: &str, root: Option<&str>) -> Relation {
        Relation {
            oid,
            schema: "public".to_owned(),
            name: name.to_owned(),
            kind: RelationKind::from_relkind(relkind),
            table: QualifiedName::new("public", table),
            root: root.map(|root| QualifiedName::new("public", root)),
            scans: None,
            enforces: false,
        }
    }

    fn lock(oid: u32, mode: &str, fastpath: bool) -> Held {
        Held {
            locktype: "relation".to_owned(),
            relation: Some(oid),
            mode: mode.to_owned(),
            granted: true,
            fastpath,
        }
    }

    fn capture(stage: Stage, held: Vec<Held>, relations: Vec<Relation>) -> Capture {
        Capture {
            stage,
            held,
            relations,
            others: Vec::new(),
            fast_path_slots: 16,
            max_locks_per_transaction: Some(64),
            server_version: 160_004,
            stats_reset: None,
            waits: None,
        }
    }

    #[test]
    fn modes_conflict_as_documented() {
        use LockMode::*;
        // Symmetric.
        for a in LockMode::ALL {
            for b in LockMode::ALL {
                assert_eq!(a.conflicts_with(b), b.conflicts_with(a), "{a:?} {b:?}");
            }
        }
        assert!(!AccessShare.conflicts_with(Exclusive));
        assert!(AccessShare.conflicts_with(AccessExclusive));
        assert!(RowExclusive.conflicts_with(Share));
        assert!(!RowExclusive.conflicts_with(RowExclusive));
        assert!(!RowExclusive.conflicts_with(ShareUpdateExclusive));
        assert!(ShareUpdateExclusive.conflicts_with(ShareUpdateExclusive));
        assert!(!Share.conflicts_with(Share));
        assert_eq!(LockMode::parse("RowExclusiveLock"), Some(RowExclusive));
        assert_eq!(LockMode::parse("SIReadLock"), None);
        assert!(RowExclusive.weak() && !Share.weak());
    }

    #[test]
    fn sizes_the_fast_path_by_version() {
        assert_eq!(fast_path_slots(170_000, Some(1024)), 16);
        assert_eq!(fast_path_slots(180_000, Some(64)), 64);
        assert_eq!(fast_path_slots(180_000, Some(100)), 128);
        assert_eq!(fast_path_slots(180_000, Some(10)), 16);
        assert_eq!(fast_path_slots(180_000, Some(1_000_000)), 16_384);
    }

    /// A partitioned table planned with a condition the planner cannot
    /// evaluate: every partition and index locked, ten of them outside the
    /// fast path.
    fn partitioned() -> (Capture, Plan) {
        let mut relations = vec![
            relation(1, "events", "p", "events", None),
            relation(2, "events_created_at_idx", "I", "events", None),
        ];
        let mut held = vec![
            lock(1, "AccessShareLock", true),
            lock(2, "AccessShareLock", true),
        ];
        for month in 1..=12 {
            let table = format!("events_2025_{month:02}");
            let index = format!("{table}_created_at_idx");
            let (table_oid, index_oid) = (10 + 2 * month, 11 + 2 * month);
            relations.push(relation(table_oid, &table, "r", &table, Some("events")));
            let mut index_relation = relation(index_oid, &index, "i", &table, Some("events"));
            index_relation.scans = Some(if month == 12 { 0 } else { 5 });
            relations.push(index_relation);
            // 14 of 26 locks get a fast-path slot.
            held.push(lock(table_oid, "AccessShareLock", month <= 7));
            held.push(lock(index_oid, "AccessShareLock", month <= 7));
        }
        held.push(Held {
            locktype: "virtualxid".to_owned(),
            relation: None,
            mode: "ExclusiveLock".to_owned(),
            granted: true,
            fastpath: true,
        });
        let plan = crate::parse(
            r#"[{"Plan": {"Node Type": "Append", "Subplans Removed": 11, "Plans": [
                {"Node Type": "Index Scan", "Parent Relationship": "Member", "Relation Name": "events_2025_12", "Schema": "public", "Alias": "events_1", "Index Name": "events_2025_12_created_at_idx"}
            ]}}]"#,
        )
        .unwrap();
        (capture(Stage::Ran, held, relations), plan)
    }

    #[test]
    fn counts_locks_outside_the_fast_path() {
        let (capture, plan) = partitioned();
        let footprint = footprint(&capture, &plan);
        assert_eq!(footprint.relation_locks, 26);
        assert_eq!(footprint.outside_fast_path, 10);
        assert_eq!(
            footprint.summary,
            "26 relation locks on 1 table, 10 of them outside the fast path (16 slots)."
        );
        let events = &footprint.tables[0];
        assert_eq!(events.table, "events");
        assert_eq!(events.partitions, 12);
        assert_eq!(events.partitions_in_plan, Some(1));
        assert_eq!(events.indexes, 13);
        assert_eq!(events.indexes_in_plan, 1);
        // The plan uses the only index never scanned.
        assert!(events.never_scanned.is_empty());
        assert_eq!(
            events.describe(),
            "26 locks: AccessShareLock on the table, 12 partitions (the plan has 1), 13 indexes (the plan uses 1); 10 outside the fast path"
        );
        let fast_path = &footprint.notes[0];
        assert_eq!(fast_path.severity, Severity::Medium);
        assert!(fast_path.summary.starts_with(
            "10 of the 26 relation locks did not fit in the backend's 16 fast-path slots."
        ));
        assert!(
            fast_path
                .action
                .as_deref()
                .unwrap()
                .contains("let the planner rule out partitions")
        );
        let partitions = &footprint.notes[1];
        assert!(partitions.summary.starts_with(
            "The statement locks 12 partitions of events and 13 indexes, but the plan has 1 of them"
        ));
        assert_eq!(
            footprint.blocked_by,
            vec![
                "ALTER TABLE (most forms), DROP, TRUNCATE, REINDEX, CLUSTER, VACUUM FULL, LOCK TABLE on events".to_owned(),
                "REINDEX, DROP INDEX or ALTER INDEX of any of the 13 indexes it locks, 12 of which the plan does not use: the planner locks every index of a table it plans".to_owned(),
            ]
        );
        // The transaction's own virtual ID is not the statement's.
        assert!(footprint.other.is_empty());
    }

    #[test]
    fn names_the_generic_plans_cost() {
        let (mut generic, plan) = partitioned();
        generic.stage = Stage::Generic;
        let custom = capture(
            Stage::Custom,
            vec![
                lock(1, "AccessShareLock", true),
                lock(34, "AccessShareLock", true),
                lock(35, "AccessShareLock", true),
            ],
            generic.relations.clone(),
        );
        let mut footprints = vec![footprint(&generic, &plan), footprint(&custom, &plan)];
        compare_executions(&mut footprints);
        assert!(
            footprints[0]
                .summary
                .starts_with("Each execution of the generic plan: 26 relation locks on 1 table")
        );
        let notes: Vec<&str> = footprints[0]
            .notes
            .iter()
            .map(|note| note.summary.as_str())
            .collect();
        assert!(notes.iter().any(|note| note.starts_with(
            "Each execution of the generic plan locks 12 partitions of events and 13 indexes, whatever the values"
        )));
        assert!(notes.iter().any(|note| note.starts_with(
            "Each execution of the generic plan takes 26 relation locks; a custom plan for the same values takes 3."
        )));
        assert_eq!(footprints[1].relation_locks, 3);
    }

    #[test]
    fn suggests_dropping_indexes_nothing_uses() {
        let mut orders_status = relation(3, "orders_status_idx", "i", "orders", None);
        orders_status.scans = Some(0);
        let mut orders_pkey = relation(2, "orders_pkey", "i", "orders", None);
        orders_pkey.scans = Some(0);
        orders_pkey.enforces = true;
        let capture = capture(
            Stage::Planned,
            vec![
                lock(1, "RowExclusiveLock", true),
                lock(2, "RowExclusiveLock", true),
                lock(3, "RowExclusiveLock", true),
            ],
            vec![
                relation(1, "orders", "r", "orders", None),
                orders_pkey,
                orders_status,
            ],
        );
        let plan = crate::parse(
            r#"[{"Plan": {"Node Type": "ModifyTable", "Operation": "Update", "Relation Name": "orders", "Schema": "public", "Alias": "orders", "Plans": [
                {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "orders", "Schema": "public", "Alias": "orders"}
            ]}}]"#,
        )
        .unwrap();
        let footprint = footprint(&capture, &plan);
        assert_eq!(footprint.tables[0].never_scanned, vec!["orders_status_idx"]);
        assert!(footprint.notes[0].summary.contains(
            "orders_status_idx is not used by this plan and not scanned since the statistics began"
        ));
        // A data change waits for CREATE INDEX and schema changes.
        assert!(footprint.blocked_by[0].starts_with(
            "ALTER TABLE (most forms), DROP, TRUNCATE, REINDEX, CLUSTER, VACUUM FULL, LOCK TABLE; CREATE TRIGGER, ALTER TABLE … ADD FOREIGN KEY; CREATE INDEX without CONCURRENTLY on orders"
        ));
    }

    #[test]
    fn reports_sessions_in_the_way() {
        let mut capture = capture(
            Stage::Ran,
            vec![lock(1, "AccessShareLock", true)],
            vec![relation(1, "orders", "r", "orders", None)],
        );
        capture.others = vec![
            Other {
                pid: Some(4521),
                relation: 1,
                mode: "AccessExclusiveLock".to_owned(),
                granted: false,
                application: Some("migrate".to_owned()),
                state: Some("active".to_owned()),
                query: Some("ALTER TABLE orders ADD COLUMN note2 text".to_owned()),
                seconds: Some(3.5),
            },
            // Does not conflict with reading.
            Other {
                pid: Some(4522),
                relation: 1,
                mode: "RowExclusiveLock".to_owned(),
                granted: true,
                application: None,
                state: None,
                query: None,
                seconds: None,
            },
        ];
        capture.waits = Some(Waits {
            blockers: vec![Blocker {
                pid: 4521,
                application: Some("migrate".to_owned()),
                state: Some("active".to_owned()),
                query: Some("ALTER TABLE orders ADD COLUMN note2 text".to_owned()),
            }],
            ..Waits::new(
                10,
                10.0,
                BTreeMap::from([("CPU".to_owned(), 4), ("Lock:relation".to_owned(), 6)]),
            )
        });
        let footprint = footprint(
            &capture,
            &crate::parse("Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)").unwrap(),
        );
        assert_eq!(footprint.conflicts.len(), 1);
        assert_eq!(
            footprint.waits.as_ref().unwrap().profile(),
            "60% Lock:relation, 40% CPU"
        );
        let summaries: Vec<&str> = footprint
            .notes
            .iter()
            .map(|note| note.summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            vec![
                "Session 4521 (migrate, active: ALTER TABLE orders ADD COLUMN note2 text) has waited for 3.50 s for AccessExclusiveLock on orders, behind sessions such as this one: until it gets the lock, every new run of the statement waits behind it.",
                "While it ran, the statement waited for another session's lock in 60% of the samples (about 60 ms): its time includes that wait, which is not the plan's. Held by session 4521 (migrate, active: ALTER TABLE orders ADD COLUMN note2 text).",
            ]
        );
    }

    #[test]
    fn lists_locks_on_other_things() {
        let mut held = vec![lock(1, "RowExclusiveLock", true)];
        held.push(Held {
            locktype: "transactionid".to_owned(),
            relation: None,
            mode: "ExclusiveLock".to_owned(),
            granted: true,
            fastpath: false,
        });
        let capture = capture(
            Stage::Ran,
            held,
            vec![relation(1, "orders", "r", "orders", None)],
        );
        let footprint = footprint(
            &capture,
            &crate::parse("Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)").unwrap(),
        );
        assert_eq!(footprint.relation_locks, 1);
        assert_eq!(footprint.outside_fast_path, 0);
        assert!(footprint.other[0].starts_with("its transaction ID (ExclusiveLock)"));
        assert_eq!(footprint.summary, "1 relation lock on 1 table.");
    }
}
