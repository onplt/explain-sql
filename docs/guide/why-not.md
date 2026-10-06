# Ask the planner why

A plan shows what the planner chose, never what it turned down. You see a sequential scan and know there is an index on that table. You see a nested loop that runs 50,000 times. You see a sort spilling to disk. Is the planner wrong, or does it know something you do not?

ExplainSQL finds out by asking it again, with its choice taken away, and comparing the two plans.

```sh
explainsql -d shop -f slow.sql --print --why-not             # the slowest nodes
explainsql -d shop -f slow.sql --print --why-not orders      # the scans of a table, or of an index's table
explainsql -d shop -f slow.sql --print --why-not --measure --runs 3
```

In the viewer, select a node and press `y`.

## What it asks

| The plan chose | ExplainSQL plans it again with | And tells you |
|---|---|---|
| A sequential scan with a condition | `enable_seqscan = off` | Whether any index can serve the condition at all, and if not, why not. |
| A nested loop on the hot path | `enable_nestloop = off` | Whether a hash or merge join is possible, and whether it is better. |
| A sort or hash that spilled (with `--measure`) | `work_mem` large enough to stay in memory | Whether more memory actually makes the statement faster. |

Without a table name, `--why-not` asks about the hot nodes, at most three: sequential scans with a condition that take 10% or more of the runtime, nested loops that [ES005](../rules/ES005.md) flags or that follow an underestimated outer side, and, with `--measure`, sorts and hashes that spilled. With a name, it asks about the sequential scans of that table, or of the table an index belongs to.

## The answers

**`UNUSABLE`: no index can serve the condition.** Even with sequential scans off, the planner still reads the whole table. ExplainSQL looks at the condition and the catalog to tell you why, which is usually the most useful thing on this page:

- the condition casts the column or wraps it in a function, so an index on the plain column does not apply (`WHERE created_at::date = …`, `WHERE lower(email) = …`);
- it ORs conditions on different columns;
- it uses `<>`, or an operator the index's type does not serve (`@>` needs GIN, not b-tree);
- it is a `LIKE` pattern that starts with a wildcard, or a prefix `LIKE` on a column whose collation is not C, without a `text_pattern_ops` index;
- the indexes on the table start with another column, are invalid, or are partial with a predicate that does not match.

**`COSTLIER`: the alternative is possible, but the planner estimates it more expensive.** The answer says by how much. Within 10% is a close call that a small change in the data can flip. Without `--measure`, ExplainSQL also checks whether `random_page_cost = 1.1`, a common value for SSDs and cloud volumes, would make the planner choose the index by itself.

**With `--measure`, it runs both plans** the same number of times (`--runs N`), after one run that warms the cache, and compares them, pages first. Then it can tell you whether the planner was right:

- **`RIGHT`**: the alternative is not better. The planner knew what it was doing.
- **`MISESTIMATE`**: the alternative is better, and the scan's rows were overestimated tenfold or more (for a nested loop, its input was underestimated). Fix the statistics: `ANALYZE`, a higher statistics target, or `CREATE STATISTICS` for correlated columns.
- **`COST MODEL`**: the alternative is better, the estimates were close, and with `random_page_cost = 1.1` the planner picks the index on its own. That plan runs too, and the setting is suggested only if it is measured better as well.
- **`WRONG`**: the alternative is better, for a reason ExplainSQL could not pin down.
- **`HELPS`** or **`NO HELP`**, for a spill: whether enough `work_mem` to stay in memory makes the statement faster.
- **`UNSURE`**: the plans compare both ways (fewer pages but slower, say), or could not be matched.

An alternative that runs past the statement timeout, while the planner's choice finished, counts as the planner being right. For a spill, the question is time: staying in memory but running slower does not help.

When the planner did not use an index that exists, the advice shows the reason found here instead of the usual list of possible reasons.

## Here is one

```text
Why not: the planner asked again (1)

  UNUSABLE     Why does Seq Scan on orders o not use an index?
               No index can serve the condition of Seq Scan on orders o: even with sequential scans
               off, the planner still reads all of orders.
               Why: no index on orders starts with customer_id
               → Create an index that serves the condition: see the advice.
               Planned again with enable_seqscan = off; estimated.
```

## Safety and limits

The settings come from a fixed list of planner settings only (the `enable_*` switches, the cost constants, `work_mem`, `hash_mem_multiplier`, `effective_cache_size`, the collapse limits, `plan_cache_mode` and `jit`). They are set with `SET LOCAL` semantics inside the transaction that is rolled back, so they never outlast the run and never touch other sessions.

`enable_*` settings apply to the whole statement, so other scans and joins can change too when one is turned off. When that happens, the answer lists the other changes and is marked approximate.

Before PostgreSQL 18, the planner adds a huge penalty (10¹⁰ per node) to plans that use a disabled node; ExplainSQL takes it out before comparing costs.
