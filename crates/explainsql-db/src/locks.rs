//! Reading what a statement locks, and watching what it waits on.
//!
//! Locks are read from explainsql's own backend inside the transaction
//! that is rolled back, after the `EXPLAIN` and before the rollback that
//! releases them. Before the statement, the transaction holds only the
//! lock on its own virtual transaction ID, which is left out. The locked
//! relations are then described from the catalog, and other sessions'
//! locks on them read from `pg_locks`. Everything here only reads.
//!
//! A second connection watches a run: it samples `pg_stat_activity` for
//! the backend and its parallel workers every 10 ms while the statement
//! runs, and notes who holds a lock the statement waits for.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use explainsql_core::locks::{
    Blocker, Capture, Held, Other, QualifiedName, Relation, RelationKind, Stage, Waits,
    fast_path_slots,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_postgres::Client;

/// The locks of explainsql's own backend, but the one every transaction
/// holds on its own virtual transaction ID.
const HELD: &str = "
SELECT locktype, relation, mode, granted, fastpath
FROM pg_catalog.pg_lock_status()
WHERE pid = pg_catalog.pg_backend_pid()
  AND NOT (locktype = 'virtualxid' AND virtualxid = virtualtransaction)";

/// The locks the statement took, with what the catalog says about them.
pub(crate) async fn capture(
    client: &Client,
    stage: Stage,
    server_version: u32,
) -> Result<Capture, tokio_postgres::Error> {
    // First, before anything else here takes locks of its own.
    let rows = client.query(HELD, &[]).await?;
    let held: Vec<Held> = rows
        .iter()
        .map(|row| Held {
            locktype: row.get(0),
            relation: row.get(1),
            mode: row.get(2),
            granted: row.get(3),
            fastpath: row.get(4),
        })
        .collect();
    let oids: Vec<u32> = held
        .iter()
        .filter(|lock| lock.locktype == "relation")
        .filter_map(|lock| lock.relation)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let relations = client
        .query(&relations_query(server_version), &[&oids])
        .await?
        .iter()
        .map(|row| Relation {
            oid: row.get(0),
            schema: row.get(1),
            name: row.get(2),
            kind: RelationKind::from_relkind(&row.get::<_, String>(3)),
            table: QualifiedName::new(&row.get::<_, String>(4), &row.get::<_, String>(5)),
            root: match (
                row.get::<_, Option<String>>(6),
                row.get::<_, Option<String>>(7),
            ) {
                (Some(schema), Some(name)) => Some(QualifiedName::new(&schema, &name)),
                _ => None,
            },
            scans: row.get(8),
            enforces: row.get(9),
        })
        .collect();
    let others = client
        .query(&others_query(server_version), &[&oids])
        .await?
        .iter()
        .map(|row| Other {
            pid: row.get(0),
            relation: row.get(1),
            mode: row.get(2),
            granted: row.get(3),
            application: row
                .get::<_, Option<String>>(4)
                .filter(|name| !name.is_empty()),
            state: row.get(5),
            query: row.get(6),
            seconds: row.get(7),
        })
        .collect();
    let settings = client
        .query_one(
            "SELECT current_setting('max_locks_per_transaction')::int4,
                    (SELECT stats_reset::text FROM pg_catalog.pg_stat_database
                     WHERE datname = pg_catalog.current_database())",
            &[],
        )
        .await?;
    let max_locks: Option<i32> = settings.get(0);
    let max_locks_per_transaction = max_locks.and_then(|locks| u32::try_from(locks).ok());
    Ok(Capture {
        stage,
        held,
        relations,
        others,
        fast_path_slots: fast_path_slots(server_version, max_locks_per_transaction),
        max_locks_per_transaction,
        server_version,
        stats_reset: settings.get(1),
        waits: None,
    })
}

/// Each locked relation, the table it belongs to (the indexed table of an
/// index, the main table of a TOAST table or its index), the partitioned
/// table at the top of that table's tree, and an index's scans.
fn relations_query(server_version: u32) -> String {
    let root = if server_version >= 120_000 {
        "CASE WHEN t.relispartition THEN pg_catalog.pg_partition_root(t.oid) END"
    } else {
        "NULL::oid"
    };
    format!(
        "
WITH locked AS (
    SELECT c.oid, c.relname, c.relnamespace, c.relkind,
           CASE WHEN c.relkind IN ('i', 'I') THEN x.indrelid ELSE c.oid END AS owner,
           coalesce(x.indisprimary OR x.indisunique OR x.indisexclusion, false) AS enforces
    FROM pg_catalog.pg_class c
    LEFT JOIN pg_catalog.pg_index x ON x.indexrelid = c.oid
    WHERE c.oid = ANY($1)
)
SELECT l.oid, n.nspname::text, l.relname::text, l.relkind::text,
       tn.nspname::text, t.relname::text, rn.nspname::text, r.relname::text,
       s.idx_scan, l.enforces
FROM locked l
JOIN pg_catalog.pg_namespace n ON n.oid = l.relnamespace
JOIN pg_catalog.pg_class t ON t.oid = coalesce(
    (SELECT m.oid FROM pg_catalog.pg_class m WHERE m.reltoastrelid = l.owner LIMIT 1),
    l.owner)
JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace
LEFT JOIN pg_catalog.pg_class r ON r.oid = {root}
LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = r.relnamespace
LEFT JOIN pg_catalog.pg_stat_all_indexes s ON s.indexrelid = l.oid"
    )
}

/// Other sessions' locks on the relations, held or waited for, and what
/// those sessions are doing. A prepared transaction has no session.
fn others_query(server_version: u32) -> String {
    let since = if server_version >= 140_000 {
        "coalesce(l.waitstart, a.xact_start)"
    } else {
        "a.xact_start"
    };
    format!(
        "
SELECT l.pid, l.relation, l.mode, l.granted, a.application_name::text, a.state::text,
       left(a.query, 300),
       extract(epoch FROM pg_catalog.clock_timestamp() - {since})::float8
FROM pg_catalog.pg_locks l
LEFT JOIN pg_catalog.pg_stat_activity a ON a.pid = l.pid
WHERE l.locktype = 'relation' AND l.relation = ANY($1)
  AND l.pid IS DISTINCT FROM pg_catalog.pg_backend_pid()
  AND l.database = (SELECT oid FROM pg_catalog.pg_database
                    WHERE datname = pg_catalog.current_database())
ORDER BY l.granted DESC, l.pid"
    )
}

/// How often a watched run is sampled.
const EVERY: Duration = Duration::from_millis(10);

/// A second connection that watches explainsql's backend while it runs a
/// statement.
#[derive(Clone)]
pub(crate) struct Watch {
    pub client: Arc<Client>,
    pub server_version: u32,
}

/// Sampling in progress.
pub(crate) struct Sampler {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<Waits>,
}

impl Watch {
    /// Starts sampling the backend `pid`, until [`Sampler::stop`]. The
    /// backend is asked for its PID in each transaction: behind a pooler,
    /// one transaction may run on another than the last.
    pub(crate) fn start(&self, pid: i32) -> Sampler {
        let (stop, stopped) = oneshot::channel();
        let watch = self.clone();
        Sampler {
            stop,
            handle: tokio::spawn(async move { watch.sample(pid, stopped).await }),
        }
    }

    async fn sample(self, pid: i32, mut stopped: oneshot::Receiver<()>) -> Waits {
        // Parallel workers name their leader from PostgreSQL 13.
        let query = if self.server_version >= 130_000 {
            "SELECT pid, state, wait_event_type, wait_event,
                    CASE WHEN wait_event_type = 'Lock' THEN pg_catalog.pg_blocking_pids(pid) END
             FROM pg_catalog.pg_stat_activity WHERE pid = $1 OR leader_pid = $1"
        } else {
            "SELECT pid, state, wait_event_type, wait_event,
                    CASE WHEN wait_event_type = 'Lock' THEN pg_catalog.pg_blocking_pids(pid) END
             FROM pg_catalog.pg_stat_activity WHERE pid = $1"
        };
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        let mut blockers: BTreeMap<i32, Blocker> = BTreeMap::new();
        let (mut ticks, mut samples) = (0_u32, 0_u32);
        let mut first: Option<Instant> = None;
        if let Ok(statement) = self.client.prepare(query).await {
            let mut ticker = tokio::time::interval(EVERY);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    _ = ticker.tick() => {}
                }
                let Ok(rows) = self.client.query(&statement, &[&pid]).await else {
                    break;
                };
                ticks += 1;
                first.get_or_insert_with(Instant::now);
                let mut active = false;
                for row in &rows {
                    // Between statements, the backend is idle in its
                    // transaction.
                    if row.get::<_, Option<String>>(1).as_deref() != Some("active") {
                        continue;
                    }
                    active = true;
                    let kind: Option<String> = row.get(2);
                    let event: Option<String> = row.get(3);
                    let name = match (kind, event) {
                        (Some(kind), Some(event)) => format!("{kind}:{event}"),
                        _ => "CPU".to_owned(),
                    };
                    *counts.entry(name).or_default() += 1;
                    let holding: Option<Vec<i32>> = row.get(4);
                    for pid in holding.unwrap_or_default() {
                        if let Entry::Vacant(entry) = blockers.entry(pid) {
                            entry.insert(self.blocker(pid).await);
                        }
                    }
                }
                if active {
                    samples += 1;
                }
            }
        }
        // The first sample is taken at once, the others an interval apart,
        // or more when a sample takes longer.
        let interval_ms = match first {
            Some(first) if ticks > 1 => {
                first.elapsed().as_secs_f64() * 1000.0 / f64::from(ticks - 1)
            }
            _ => EVERY.as_secs_f64() * 1000.0,
        };
        Waits {
            blockers: blockers.into_values().collect(),
            ..Waits::new(samples, interval_ms, counts)
        }
    }

    /// What a session holding a lock the statement waits for is doing.
    async fn blocker(&self, pid: i32) -> Blocker {
        let row = self
            .client
            .query_opt(
                "SELECT application_name::text, state::text, left(query, 300)
                 FROM pg_catalog.pg_stat_activity WHERE pid = $1",
                &[&pid],
            )
            .await
            .ok()
            .flatten();
        Blocker {
            pid,
            application: row
                .as_ref()
                .and_then(|row| row.get::<_, Option<String>>(0))
                .filter(|name| !name.is_empty()),
            state: row.as_ref().and_then(|row| row.get(1)),
            query: row.as_ref().and_then(|row| row.get(2)),
        }
    }
}

impl Sampler {
    /// Stops sampling and returns what was seen.
    pub(crate) async fn stop(self) -> Option<Waits> {
        let _ = self.stop.send(());
        self.handle.await.ok()
    }
}

/// A measured run that waited for another session's lock is run again, at
/// most this many times.
const RETRIES: usize = 2;

/// Whether to run a measured run again because it waited for another
/// session's lock, `tries` times so far, with a note for the user.
pub(crate) fn again(waits: Option<&Waits>, tries: &mut usize, notes: &mut Vec<String>) -> bool {
    let Some(waits) = waits else {
        return false;
    };
    if waits.lock_samples() == 0 {
        return false;
    }
    let by: Vec<String> = waits
        .blockers
        .iter()
        .map(|blocker| match &blocker.application {
            Some(application) => format!("session {} ({application})", blocker.pid),
            None => format!("session {}", blocker.pid),
        })
        .collect();
    let lock = if by.is_empty() {
        "another session's lock".to_owned()
    } else {
        format!("a lock held by {}", by.join(", "))
    };
    let waited = waits.waited();
    *tries += 1;
    if *tries <= RETRIES {
        notes.push(format!(
            "a measured run waited about {waited} for {lock}, so it was run again"
        ));
        true
    } else {
        notes.push(format!(
            "a measured run waited for {lock} on each of {} tries, about {waited} the last time: its time includes that wait",
            RETRIES + 1
        ));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_again_a_run_that_waited_for_a_lock() {
        let mut counts = BTreeMap::new();
        counts.insert("Lock:relation".to_owned(), 30);
        counts.insert("CPU".to_owned(), 10);
        let waits = Waits {
            blockers: vec![Blocker {
                pid: 4521,
                application: Some("psql".to_owned()),
                state: Some("idle in transaction".to_owned()),
                query: None,
            }],
            ..Waits::new(40, 10.0, counts)
        };
        let (mut tries, mut notes) = (0, Vec::new());
        assert!(again(Some(&waits), &mut tries, &mut notes));
        assert!(again(Some(&waits), &mut tries, &mut notes));
        assert!(!again(Some(&waits), &mut tries, &mut notes));
        assert_eq!(
            notes,
            [
                "a measured run waited about 300 ms for a lock held by session 4521 (psql), so it was run again",
                "a measured run waited about 300 ms for a lock held by session 4521 (psql), so it was run again",
                "a measured run waited for a lock held by session 4521 (psql) on each of 3 tries, about 300 ms the last time: its time includes that wait",
            ]
        );
        // Not watched, or no wait for a lock.
        assert!(!again(None, &mut 0, &mut notes));
        let busy = Waits::new(5, 10.0, BTreeMap::from([("CPU".to_owned(), 5)]));
        assert!(!again(Some(&busy), &mut 0, &mut notes));
        assert_eq!(notes.len(), 3);
    }
}
