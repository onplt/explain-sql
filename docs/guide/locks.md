# What a statement locks

`EXPLAIN` never shows locks, yet locks are behind some of the nastiest production incidents: a migration that hangs behind a long query and takes every later query down with it, or hundreds of sessions slowing each other down on the lock manager because each one locks 200 partitions. `--locks` shows you what a statement locks before that happens.

```sh
explainsql -d shop -f report.sql --locks --print
explainsql -d shop -c 'SELECT * FROM events WHERE created_at > $1' --bind 1=2025-12-20 --locks --print
```

In the viewer, press `L`.

## How it works

When ExplainSQL runs a statement in [connected mode](connected.md), the transaction it is about to roll back still holds every lock the statement took. So ExplainSQL reads them right there, from `pg_lock_status()`, just before the rollback releases them. It also reads `pg_locks` and `pg_stat_activity` for other sessions' locks on the same relations, and, from a second connection, watches what the statement waits for while it runs.

## What the report tells you

Here is a statement that counts the last 30 days of a table partitioned by month:

```text
Locks the statement takes
  26 relation locks on 1 table, 10 of them outside the fast path (16 slots).
  events  26 locks: AccessShareLock on the table, 12 partitions (the plan has none), 13 indexes (the
          plan uses none); 10 outside the fast path

  MEDIUM  10 of the 26 relation locks did not fit in the backend's 16 fast-path slots. …

  MEDIUM  The statement locks 12 partitions of events and 13 indexes, but the plan has none of them:
          the planner could not rule the others out when planning, and run-time pruning dropped them
          after they were locked.
          → Compare the partition key with a constant, or a parameter of a custom plan, rather than
          an expression the planner cannot evaluate, such as one of now(): …
```

In detail:

- **How many relation locks the statement takes, table by table**: the table, its partitions and its indexes. The planner locks every index of every table it plans, used or not, and every partition it cannot rule out while planning, such as when the partition key is compared with `now()`.
- **How many fall outside the fast path.** A backend takes weak relation locks (`AccessShareLock`, `RowShareLock`, `RowExclusiveLock`) in fast-path slots of its own: 16 of them before PostgreSQL 18, and from 18 as many as `max_locks_per_transaction` allows, 64 by default. Locks that do not fit go to the shared lock table. When many sessions run such a statement at once, they contend for it (wait event `LWLock:LockManager`) and slow each other down. The cure is to take fewer locks: drop the indexes nothing uses, or let the planner rule out partitions.
- **Indexes nothing uses**: locked by every run, not used by this plan, not scanned since the statistics were reset, and not enforcing a constraint. Replicas count their own scans, so check theirs before you drop one.
- **What would wait for these locks**: the commands whose locks conflict with the statement's, such as `ALTER TABLE` on its tables, or `REINDEX` of any index it locks, including those the plan does not use. While such a command waits for a long statement, every later run of the statement queues up behind it. That is why the report recommends a `lock_timeout` for schema changes.
- **Other sessions' locks that conflict right now**, such as a migration that is already waiting behind statements like this one.
- **What the statement waited on as it ran.** The second connection samples `pg_stat_activity` every 10 ms while `EXPLAIN ANALYZE` runs, for the backend and its parallel workers. If the statement waited for another session's lock, the report says for how long and who held it, because that time belongs to the other session, not to the plan.

With `--no-analyze`, you see the locks that planning takes; `EXPLAIN ANALYZE` adds those of running.

## Statements with parameters

With `--params` or `--bind`, the report shows the locks of one execution of the generic plan and of one custom plan, with the parameters held at their typical values. This matters for partitioned tables. PostgreSQL does not plan a cached generic plan again: it locks every partition in it before run-time pruning drops the ones the values rule out. A custom plan locks only the partitions the planner keeps. So as partitions pile up, every execution of the generic plan takes more and more locks.

To see this, ExplainSQL prepares the statement and makes its plan in one transaction, then executes it in a second one, whose locks it reads. Without `--measure`, the plans do not run, so the generic plan's count leaves out the indexes its scans would open.

## Runs that waited

With `--locks`, `--measure` or `--prove`, the second connection also watches measured runs. A run that waited for another session's lock is run again, up to twice, and ExplainSQL says so, so that a lock wait is never mistaken for a slow plan.

## Safety

`--locks` only reads: `pg_lock_status()`, `pg_locks`, `pg_stat_activity` and the catalog, inside the transaction that is rolled back. Reading `pg_locks` takes the lock manager's internal locks for a moment, twice per run. The second connection uses the same settings as the first.
