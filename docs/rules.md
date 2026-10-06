# Rule catalog

Rules turn plan data into findings. Every finding names its rule, the node it is about, a severity, the evidence that triggered it, and a suggested action. The bar for a rule is precision: a rule that is sometimes obviously wrong does more harm than a missing rule. So each rule also says when it deliberately stays silent.

The index advisor builds its `CREATE INDEX` suggestions on the findings of ES001, ES005, ES006 and ES009 (see "Index advisor" in [ARCHITECTURE.md](ARCHITECTURE.md)).

Each rule lives in its own file, `crates/explainsql-core/src/rules/esNNN_*.rs`, with its thresholds as named constants; they will become configurable. Every plan in the fixture corpus is checked against the rules its scenario expects, and no other rule may fire (see [fixtures/README.md](https://github.com/onplt/explain-sql/blob/HEAD/fixtures/README.md)). Each rule has a page with its thresholds, when it stays silent, and an example from the corpus. To propose a new rule, see [contributing](contributing.md#adding-or-changing-a-rule).

**Severity** follows the share of the runtime involved: half or more is high, a fifth or more is medium, anything less is low. When the plan has no timing, the share is taken from buffers. Misestimates are low on their own and at least medium when they feed a join.

**Shares** are exclusive: the time spent in a node itself, as a fraction of the statement's execution time. See "Metrics" in [ARCHITECTURE.md](ARCHITECTURE.md) for how parallel workers, CTEs, InitPlans and rounding are accounted for.

| ID | Name | Suggested action |
|---|---|---|
| [ES001](rules/ES001.md) | Selective sequential scan | An index on the filtered columns, or rewriting a condition that wraps the column |
| [ES002](rules/ES002.md) | Row misestimate | `ANALYZE`, `CREATE STATISTICS`, statistics target |
| [ES003](rules/ES003.md) | Sort spilled to disk | A `work_mem` for the statement, with what it may take, or an index matching the sort |
| [ES004](rules/ES004.md) | Hash or aggregate spilled to disk | A `work_mem` for the statement, with what it may take, or `hash_mem_multiplier` |
| [ES005](rules/ES005.md) | Expensive nested-loop inner side | Index on the inner join key |
| [ES006](rules/ES006.md) | Index scan that filters most rows | Composite index |
| [ES007](rules/ES007.md) | Index-only scan with many heap fetches | `VACUUM` (visibility map) |
| [ES008](rules/ES008.md) | Lossy bitmap or heavy recheck | `work_mem` |
| [ES009](rules/ES009.md) | Slow foreign-key trigger | Index on the referencing columns |
| [ES010](rules/ES010.md) | Cartesian product | Add the missing join condition |
| [ES011](rules/ES011.md) | Fewer parallel workers than planned | Review the parallel worker pool |
| [ES012](rules/ES012.md) | JIT overhead dominates | `jit_above_cost`, or `jit = off` for OLTP |
| [ES013](rules/ES013.md) | Planner settings force the plan | Reset `enable_*` settings left off in the session, role or database |
