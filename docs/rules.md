# Rule catalog

Rules turn plan data into findings. Every finding names its rule, the node it is about, a severity, the evidence that triggered it, and a suggested action. The bar for a rule is precision: a rule that is sometimes obviously wrong does more harm than a missing rule. So each rule also says when it deliberately stays silent.

The index advisor builds its `CREATE INDEX` suggestions on the findings of ES001, ES005, ES006 and ES009 (see "Index advisor" in [ARCHITECTURE.md](ARCHITECTURE.md)).

Each rule lives in its own file, `crates/explainsql-core/src/rules/esNNN_*.rs`, with its thresholds as named constants; they will become configurable. Every plan in the fixture corpus is checked against the rules its scenario expects, and no other rule may fire (see [fixtures/README.md](../fixtures/README.md)). Pages per rule will come with the documentation site.

**Severity** follows the share of the runtime involved: half or more is high, a fifth or more is medium, anything less is low. When the plan has no timing, the share is taken from buffers. Misestimates are low on their own and at least medium when they feed a join.

**Shares** are exclusive: the time spent in a node itself, as a fraction of the statement's execution time. See "Metrics" in [ARCHITECTURE.md](ARCHITECTURE.md) for how parallel workers, CTEs, InitPlans and rounding are accounted for.

| ID | Name | Suggested action |
|---|---|---|
| ES001 | Selective sequential scan | An index on the filtered columns, or rewriting a condition that wraps the column |
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

- **Signal:** a `Seq Scan` (parallel or not) that keeps less than 5% of the rows it reads, on a table of at least 1,000 pages (or 50,000 rows per scan when the plan has no buffer counts), taking at least 10% of the runtime. Rows kept are those that pass the scan's filter. When the scan is the inner side of a nested loop, they are the rows that pass the loop's join filter.
- **Evidence:** rows kept of rows read, the filter, the table size in pages, loops, and the time in the node.
- **Action:** an index on the filtered columns. The finding says when the operator needs a trigram index (`LIKE '%…'`), `text_pattern_ops` (`LIKE 'abc%'`) or GIN (`@>`, `&&`, `@@`). When the filter wraps the column in a cast or a function, an index on the column cannot help, and the action is to rewrite the condition or index the expression.
- **Stays silent when:** the table is small, the scan is cheap compared with the statement, a `Limit`, a semi or anti join, or a subquery can stop the scan early, or the filter ORs conditions on different columns (no single index serves it).

## ES002: Row misestimate

- **Signal:** actual and estimated rows per loop differ by 10× or more, and the error starts at this node rather than being passed up from a child. Each count is taken as at least one row.
- **Evidence:** estimated and actual rows, loops, and the join that consumes the node.
- **Action:** depends on the cause:
  - stale statistics: `ANALYZE` the table;
  - correlated columns in the condition: `CREATE STATISTICS` on them;
  - a single column: a higher statistics target;
  - a cast or a function of a column, which has no statistics: rewrite the condition, or `CREATE STATISTICS` on the expression (PostgreSQL 14 and later);
  - a comparison with a value known only at run time (`$1`, an InitPlan's result): a default guess that `ANALYZE` cannot change.
- **Stays silent when:**
  - both counts are small: under 100 rows per loop, and under 10,000 over all loops;
  - the node returned fewer rows than estimated but a node above may have stopped it early;
  - the node passes its input's rows through (`Sort`, `Hash`, `Materialize`, `Memoize`, `Gather`) or reports none (bitmap nodes);
  - the node is a recursive CTE's `Recursive Union`, whose depth the planner cannot know.

  A `CTE Scan` inherits the error of the CTE it reads.

## ES003: Sort spilled to disk

- **Signal:** a `Sort` or `Incremental Sort`, in the leader or in a parallel worker, whose method is `external merge` or `external sort`.
- **Evidence:** sort method, disk space used, sort key, and the time in the node.
- **Action:** raise `work_mem` for the query or the session rather than globally, to about twice the space written to disk. Alternatively, avoid the sort with an index that matches the sort order.
- **Stays silent when:** the spill is under 10 MB and the sort takes less than 5% of the runtime.

## ES004: Hash or aggregate spilled to disk

- **Signal:** a `Hash` split into more than one batch, or a hashed aggregate that reports several batches or disk usage.
- **Evidence:** batches (and the planned number), peak memory, data written to disk.
- **Action:** raise `work_mem` or `hash_mem_multiplier` for the query. The finding estimates the hash table's full size, and points to ES002 when the input of the hash was underestimated.

## ES005: Expensive nested-loop inner side

- **Signal:** a `Nested Loop` whose inner side runs at least twice. The work it repeats (everything but producing the outer rows) must take at least half of the loop's time, and the loop at least 10% of the runtime. The inner side must also be a sequential scan, or a scan that removes at least ten times the rows it keeps.
- **Evidence:** inner loops, inner time per loop, the repeated work, and the rows removed per loop.
- **Action:** an index on the inner side's join key, taken from the join filter or from the inner scan's conditions on the outer side. The finding points to ES002 when the planner expected far fewer outer rows.
- **Stays silent when:** a `Materialize` or `Memoize` caches the inner side.

## ES006: Index scan that filters most rows

- **Signal:** an `Index Scan`, `Index Only Scan` or `Bitmap Heap Scan` whose filter removes at least 90% of the rows found through the index. It must remove at least 100 rows per loop (or 1,000 over all loops), and the scan, including its index, must take at least 10% of the runtime.
- **Evidence:** the index, the index condition, the filter, and rows removed of rows found.
- **Action:** a composite index that covers the filtered columns as well as the index condition's.

## ES007: Index-only scan with many heap fetches

- **Signal:** an `Index Only Scan` with heap fetches amounting to at least 25% of the rows returned and at least 100 in total, taking at least 10% of the runtime.
- **Evidence:** heap fetches and rows returned.
- **Action:** `VACUUM` the table to update its visibility map, and check that autovacuum keeps up with how often the table changes.

## ES008: Lossy bitmap or heavy recheck

- **Signal:** a `Bitmap Heap Scan` with lossy heap blocks, taking at least 5% of the runtime.
- **Evidence:** lossy and exact blocks, rows removed by the recheck.
- **Action:** raise `work_mem` so that the bitmap stays exact.
- **Stays silent when:** rows are rechecked without any lossy blocks. That comes from lossy operator classes (trigram indexes, for example), which `work_mem` does not help.

## ES009: Slow foreign-key trigger

- **Signal:** a foreign-key action trigger on the referenced side that takes at least 20% of the execution time. That is a `RI_ConstraintTrigger_a_…` trigger, or a constraint trigger of a `DELETE` when the plan does not name it (text plans without `VERBOSE`).
- **Evidence:** trigger, constraint, calls, total time and time per call.
- **Action:** an index on the referencing columns of the child table.
- **Stays silent when:** the trigger is an `INSERT`'s check trigger (`RI_ConstraintTrigger_c_…`), which looks up the referenced table's unique index.

## ES010: Cartesian product

- **Signal:** a `Nested Loop` with no join filter that pairs each outer row with all inner rows. Its output must be at least 90% of outer rows × inner rows, and at least 1,000 rows.
- **Evidence:** outer rows, inner rows per loop, rows produced.
- **Action:** check the query for a missing join condition between the relations on either side.
- **Stays silent when:** either side returns a single row (as in a deliberate cross join with a one-row subquery), the inner side refers to the outer side (a parameterized scan), or the join is a semi or anti join.

## ES011: Fewer parallel workers than planned

- **Signal:** `Workers Launched` lower than `Workers Planned` on a `Gather` or `Gather Merge`.
- **Evidence:** planned and launched workers.
- **Action:** the worker pool was exhausted. Review `max_parallel_workers` and `max_worker_processes` against the number of parallel queries running at once.

## ES012: JIT overhead dominates

- **Signal:** JIT compilation takes at least half of the execution time.
- **Evidence:** functions compiled, the time of each compilation step, and the execution time.
- **Action:** raise `jit_above_cost`, or set `jit = off` for OLTP workloads. When inlining and optimization dominate, raising `jit_inline_above_cost` and `jit_optimize_above_cost` keeps JIT but drops its most expensive steps.
