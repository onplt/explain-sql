# What a write costs

An `UPDATE` that touches one row looks cheap in its plan. But if it is not a HOT update, it also writes a new entry into every index of the table, with the WAL that comes with each one, and leaves dead index entries for VACUUM to clean up. Do that a few thousand times a second and it adds up, on the primary, on every replica, and in your backups. The plan does not tell you any of this. ExplainSQL does.

```sh
explainsql -d shop -c "UPDATE orders SET status = 'shipped' WHERE id = 42" --allow-dml --print
explainsql -d shop -f update.sql --allow-dml --allow-ddl --prove --print
```

In the viewer, press `W` once the measured plan is in.

## How it works

With `--allow-dml`, a statement that writes runs inside the transaction that is rolled back. Just before the rollback, ExplainSQL reads what it wrote from the transaction's own counters (`pg_stat_xact_user_tables`), and the WAL it wrote from `EXPLAIN (ANALYZE, WAL)`. Then everything is rolled back as usual.

```text
Writes
  1 row updated in orders, not HOT: 2 index entries and 209 B of WAL per row.
  orders  1 updated (0 HOT); 2 index entries in 2 indexes
  WAL: 3 records, 209 B.

  LOW     The update of orders was not HOT: the statement sets created_at (orders_created_at_idx),
          which an index refers to, so when the value changes, each such update writes a new entry
          in every index of the table: 2 index entries per row.
```

## What the report tells you

- **The rows each table got**, including those written by triggers and foreign-key cascades.
- **Whether updates were HOT.** An update is HOT (a "heap-only tuple") when the new version of the row fits on the same page as the old one, and no index refers to a column whose value changed. Then no index gets a new entry at all. Otherwise every index of the table gets one, for every row, plus the WAL for each.
- **What kept them from being HOT.** The columns the statement sets (in `UPDATE … SET`, `INSERT … ON CONFLICT DO UPDATE` and `MERGE`) that an index refers to, whether in its keys, its `INCLUDE` list, its expressions or its predicate. From PostgreSQL 16, BRIN indexes do not count. If such an index has not been scanned since the statistics were reset, the finding becomes medium: dropping it would let these updates be HOT. If no index refers to any column the statement sets, the problem is room on the page: the report says how many new versions went to another page (from PostgreSQL 16) and shows the table's `fillfactor`, which you can lower to leave room.
- **Index entries and WAL per row.** From PostgreSQL 13, ExplainSQL adds `WAL` to `EXPLAIN ANALYZE` for statements that write. The first change to a page after a checkpoint writes the whole page to WAL (a full-page image). When those images are most of the records, the report says so, because running the statement again soon after writes far less.

## The proof

Like an index suggestion, the claim "this index is costing you HOT updates" can be tested. With `--prove --allow-ddl`, ExplainSQL drops the indexes that kept the updates from being HOT inside a transaction, runs the statement again there, reads what it wrote, and rolls back, which brings the indexes back:

```text
Without orders_created_at_idx (dropped in a transaction that was rolled back): 1 of 1 update HOT,
no index entries, 81 B of WAL per row.
```

It leaves out indexes that enforce a constraint, and partitions' indexes that belong to an index of the partitioned table. Dropping an index locks its table against both reads and writes until the rollback, so ExplainSQL gives up if it waits more than 2 seconds for that lock. As with every `--allow-ddl` test, use it on development or staging databases.

If the updates are still not HOT without those indexes, the report says why: the pages had no room for the new versions, or a column changed that another index refers to.

## Limits

The writes are read for a statement run as it is, not under `--params`. In the JSON report, they are under `writes`.
