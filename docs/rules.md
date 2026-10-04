# Rule catalog (planned)

Rules turn plan data into findings. Every rule has a stable ID, a severity derived from its impact on the plan, the evidence that triggered it, a suggested action, and conditions under which it deliberately stays silent. The bar for a rule is precision: a rule that is sometimes obviously wrong does more harm than a missing rule.

Each rule will get its own page under `docs/rules/` once it is implemented. The thresholds below are starting points and will be configurable.

| ID | Name | Suggested action |
|---|---|---|
| ES001 | Selective sequential scan | Index candidate from the advisor |
| ES002 | Row misestimate | `ANALYZE`, `CREATE STATISTICS`, statistics target |
| ES003 | Sort spilled to disk | Session-level `work_mem`, or an index matching the sort |
| ES004 | Hash or aggregate spilled to disk | `work_mem` / `hash_mem_multiplier` |
| ES005 | Expensive nested-loop inner side | Index on the inner join key |
| ES006 | Index scan that filters most rows | Composite index |
| ES007 | Index-only scan with many heap fetches | `VACUUM` (visibility map) |
| ES008 | Lossy bitmap or heavy recheck | `work_mem` |
| ES009 | Slow foreign-key trigger | Index on the referencing columns |
| ES010 | Cartesian product | Add the missing join condition |
| ES011 | Fewer parallel workers than planned | Review the parallel worker pool |
| ES012 | JIT overhead dominates | `jit_above_cost`, or `jit = off` for OLTP |

## ES001: Selective sequential scan

- **Signal:** a `Seq Scan` or `Parallel Seq Scan` whose filter discards most rows, with an observed selectivity `rows / (rows + Rows Removed by Filter)` below 5%.
- **Evidence:** observed selectivity, rows read vs rows returned, buffers (approximately the table size in pages), share of exclusive time, loops.
- **Action:** hand off to the index advisor for a candidate index.
- **Stays silent when:** the table is small (about 1,000 pages or less), the node is not on the hot path, a `Limit` stops the scan early, or the predicate is not sargable (a cast on the column side, or an `OR` across columns).

## ES002: Row misestimate

- **Signal:** actual and estimated rows (both per loop) differ by 10× or more, especially on the inputs of joins, where the planner's choice of join method depends on the estimate.
- **Evidence:** estimated vs actual rows, the misestimate factor, and the join that consumes the node.
- **Action:** `ANALYZE` the table; consider `CREATE STATISTICS` for correlated columns, or a higher statistics target for the column.
- **Stays silent when:** both values are tiny (for example, 1 estimated vs 8 actual), or a `Limit` above the node stopped it early.

## ES003: Sort spilled to disk

- **Signal:** `Sort Method: external merge` with `Sort Space Type: Disk`.
- **Evidence:** disk space used, sort key, share of time.
- **Action:** raise `work_mem` for the session or the query rather than globally, or avoid the sort with an index that matches the sort order.
- **Stays silent when:** the spill is small and the node is not on the hot path.

## ES004: Hash or aggregate spilled to disk

- **Signal:** a `Hash` node with more than one batch (or with `Original Hash Batches` lower than `Hash Batches`), or a `HashAggregate` reporting batches and disk usage.
- **Evidence:** batches, peak memory, disk usage.
- **Action:** raise `work_mem` or `hash_mem_multiplier` for the session, and check the build side for a row misestimate (ES002).

## ES005: Expensive nested-loop inner side

- **Signal:** a `Nested Loop` whose inner side accounts for most of its time (`loops × inner time`), with a sequential scan or a heavily filtered scan on the inner side.
- **Evidence:** outer rows (which equal inner loops), inner time per loop, rows removed on the inner side.
- **Action:** an index on the inner side's join key (from the advisor), or check for a misestimate that made the planner expect few outer rows.

## ES006: Index scan that filters most rows

- **Signal:** an `Index Scan` or `Bitmap Heap Scan` with a large `Rows Removed by Filter` relative to the rows it returns.
- **Evidence:** the index used, index condition vs filter, rows removed.
- **Action:** a composite index that also covers the filtered columns (from the advisor).

## ES007: Index-only scan with many heap fetches

- **Signal:** an `Index Only Scan` where `Heap Fetches` is a large fraction of the rows returned.
- **Evidence:** heap fetches vs rows.
- **Action:** `VACUUM` the table to update the visibility map, and review autovacuum settings for frequently updated tables.

## ES008: Lossy bitmap or heavy recheck

- **Signal:** `Heap Blocks: lossy=…` on a `Bitmap Heap Scan`, or a large `Rows Removed by Index Recheck`.
- **Evidence:** exact vs lossy blocks, rows removed by recheck.
- **Action:** raise `work_mem` so that the bitmap stays exact.

## ES009: Slow foreign-key trigger

- **Signal:** a trigger in the `Triggers` section, typically `RI_ConstraintTrigger_…`, that accounts for a large share of execution time. This is common with a `DELETE` or `UPDATE` on a referenced table.
- **Evidence:** trigger name, constraint, calls, time.
- **Action:** an index on the referencing columns of the child table (from the advisor).

## ES010: Cartesian product

- **Signal:** a `Nested Loop` with no join condition (no `Join Filter`, and an inner side that does not reference the outer side) whose output is close to outer rows × inner rows.
- **Evidence:** outer rows, inner rows, output rows.
- **Action:** check the query for a missing join predicate.
- **Stays silent when:** one side returns a single row, as in a deliberate cross join with a one-row subquery.

## ES011: Fewer parallel workers than planned

- **Signal:** `Workers Launched` lower than `Workers Planned` on a `Gather` or `Gather Merge`.
- **Evidence:** planned vs launched workers.
- **Action:** the worker pool was exhausted; review `max_parallel_workers`, `max_parallel_workers_per_gather` and `max_worker_processes`, or the concurrent parallel load.

## ES012: JIT overhead dominates

- **Signal:** total JIT time is a large share (for example, over 50%) of a short query's execution time.
- **Evidence:** JIT generation, inlining, optimization and emission times, and the execution time.
- **Action:** raise `jit_above_cost` (and the related inlining and optimization thresholds), or disable JIT for OLTP workloads.
