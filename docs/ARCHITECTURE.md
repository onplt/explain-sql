# Architecture

This document describes the planned design. Nothing here is implemented yet; it will be updated as the code lands.

## Technology choice: Rust, Ratatui and Crossterm

We seriously considered both Rust (Ratatui + Crossterm) and Go (Bubble Tea + Lip Gloss).

| Criterion | Rust + Ratatui | Go + Bubble Tea | Edge |
|---|---|---|---|
| Dense data UI (tree-table, detail pane, popups) | Cell buffer, constraint-based layout, overlays via `Clear` | Layout by string composition; dense Go TUIs often use tview instead (k9s, lazysql) | Rust |
| Parsing a schema that changes across versions | serde with `Option<T>` fields and `#[serde(flatten)]` for unknown keys | `encoding/json` with a custom `UnmarshalJSON` | Rust (slight) |
| Node types and rule engine | Enums with exhaustive `match` | Strings and `switch` | Rust |
| Testing | `insta` snapshots, plus rendered-screen snapshots via Ratatui's `TestBackend` | Golden files, teatest | Rust |
| Reusing the engine elsewhere | WebAssembly (Ratzilla runs Ratatui UIs in the browser), napi/PyO3 bindings | Heavier WebAssembly story | Rust (strategic) |
| PostgreSQL driver | tokio-postgres + rustls | pgx (excellent) | Go |
| Release tooling | cargo-dist | goreleaser | Tie |
| Learning curve | Steep (ownership, lifetimes) | Productive within weeks | Go |

**Decision: Rust.** The product is a data-heavy analysis engine with a thin UI on top. Writing the engine once and reusing it in the CLI, the TUI, a browser playground, an MCP server and an editor extension is what makes the project sustainable. Go would be the better choice only if time to the first release outweighed everything else, and the architecture below would not change.

## Workspace layout

```
explain-sql/
├─ Cargo.toml                    # workspace, shared lints and profiles
├─ crates/
│  ├─ explainsql-core/           # no I/O, no async, WASM-compatible
│  │  └─ src/{ir/, pg/{sniff,normalize,json,text,lower}.rs, metrics/, rules/, advisor/, expr/}
│  ├─ explainsql-db/             # tokio-postgres + rustls: safe executor, catalog reader, HypoPG/rollback prover
│  ├─ explainsql-tui/            # Ratatui app: state, views, keymap, theme
│  └─ explainsql/                # binary: clap CLI, mode dispatch (tui | print | pager | json)
├─ fixtures/
│  ├─ schema.sql                 # deterministic dataset
│  ├─ scenarios/<name>.sql       # one statement plus expectations (rules, advice) per scenario
│  └─ pg/{12..18}/               # generated plans: <name>.json, <name>.txt, manifest.json
├─ docs/rules/                   # one page per rule
├─ xtask/                        # gen-fixtures and check-fixtures; later an anonymizer and release helpers
└─ .github/workflows/            # ci, fixtures, release
```

We use four crates and no more. Keeping `core` free of I/O is required for WebAssembly and for fast, deterministic tests; finer splits would slow down early development.

Process model: `core` is synchronous and pure. The TUI talks to the database layer over channels. The database layer runs on a background Tokio runtime and can cancel a running query.

## Plan IR

Plans are stored in an arena: nodes live in a `Vec<Node>` and refer to each other through `NodeId(u32)` indices. This avoids ownership problems with parent pointers, is cache-friendly and serializes trivially.

```rust
pub struct Node {
    pub kind: NodeKind,              // SeqScan, HashJoin, ..., Custom(String)
    pub rel: Relationship,           // Outer | Inner | Member | InitPlan | SubPlan | Subquery
    pub subplan_name: Option<String>,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub est: Estimates,              // cost, rows, width
    pub act: Option<Actuals>,        // per-loop time/rows + loops; None = no ANALYZE or never executed
    pub buffers: Option<Buffers>,    // TOTALS across loops, not per loop
    pub predicates: Vec<Predicate>,  // Filter, Index Cond, Hash Cond, ... (raw text, parsed lazily)
    pub extra: BTreeMap<String, serde_json::Value>, // unknown keys are never dropped
    pub derived: Derived,            // inclusive/exclusive time, share of total, misestimate factor
}
```

Every metric carries its provenance (`Measured`, `Estimated`, `Derived` or `Unknown`), so the UI never presents a guess as a measurement.

The IR is engine-agnostic. PostgreSQL-specific details live in the `pg` module and are lowered into the IR; a future MySQL front end would do the same.

## Parsing pipeline

```
input ─▶ sniff() ─▶ normalize() ─▶ parse_json() | parse_text() ─▶ PgPlan ─▶ lower() ─▶ Plan IR ─▶ analyze()
```

- **JSON is the primary format and the source of truth.** When connected, we always request `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON)`, with the options adjusted to the server version.
- **The text format is supported from v0.1.** It is the default in psql and in `auto_explain`, and most plans shared in issues and chats are text. Accepting only JSON would turn away a large share of users on their first try.
- **No parser generator.** The text parser is hand-written: an indentation stack, a line classifier (node header, property line, or a section such as `Planning:`, `JIT:`, `Triggers:` or `Settings:`) and targeted regular expressions. It must tolerate damaged input.
- **Normalization** strips the wrappers that real plans arrive in: psql's aligned output (the `QUERY PLAN` header, `+` continuation markers and the `(N rows)` footer), `auto_explain` log lines with a `log_line_prefix`, CSV and JSON logs, copies from GUI clients, Markdown code fences, CRLF line endings and truncated plans.
- **Never fail hard.** Unknown properties are kept in `extra` and shown generically. If only part of a plan can be parsed, that part is rendered along with a warning.
- **Not supported:** the YAML and XML formats.

### Version and fork drift (examples)

- PostgreSQL 13: buffers used during planning; the `WAL` option.
- PostgreSQL 14: the `Memoize` node.
- PostgreSQL 16: the `GENERIC_PLAN` option.
- PostgreSQL 17: the `SERIALIZE` and `MEMORY` options; I/O timings split into shared and local; subplan outputs shown as `(InitPlan 1).col1` instead of `$0`.
- PostgreSQL 18: `BUFFERS` is on by default with `ANALYZE`; actual row counts are always printed with two decimals (`rows=10.00`, and `"Actual Rows": 10.00` in JSON, so parsers must read them as floats); `Index Searches`; `Disabled: true` on nodes the planner had to use despite an `enable_*` setting. Older versions add a huge `disable_cost` of 1e10 instead, which a naive tool mistakes for the most expensive node.
- Extensions and forks: `Custom Scan` nodes (Citus, TimescaleDB), `Motion` nodes (Greenplum). Unknown node types are rendered generically, never rejected.

### Testing the parsers

- Every fixture is generated from a real PostgreSQL server (Docker, versions 12–18) in both JSON and text form; see [fixtures/README.md](../fixtures/README.md). The two forms come from separate executions, so differential testing requires both parsers to produce the same plan shape, estimates and row counts, while timings and buffer counts may differ.
- Parsers are fuzzed with `cargo-fuzz`.
- The IR and the rendered output are snapshot-tested with `insta`.

## Metrics: inclusive and exclusive time

Getting per-node numbers right is harder than it looks, and everything else is built on it.

- **Times and rows are per-loop averages; buffers are totals across loops.** Multiply time by `loops`; never multiply buffers.
- **Parallel query.** Below a `Gather` node, `loops` counts the processes that ran the node, so `avg × loops` is CPU time, not wall-clock time. Simple subtraction then makes the Gather's exclusive time negative. We offer two modes: wall-clock (divided by the number of processes) and CPU.
- **CTEs and InitPlans.** A CTE subtree's time is counted both under the node it is listed beneath and inside the `CTE Scan` that pulls rows from it. An InitPlan's time is charged to whichever node first evaluates its parameter, which may not be the node it is displayed under. (SubPlans are correctly included in their parent.)
- **Time outside the tree.** AFTER triggers (including foreign-key checks, which can dominate a slow `DELETE`), executor startup and JIT are not attributed to plan nodes. They are shown as separate buckets: triggers, and an "unattributed" remainder.
- **Rounding.** Times are printed with 0.001 ms resolution, so with a million loops there are ±500 ms of uncertainty. Before PostgreSQL 18, per-loop row counts are rounded to integers (0.4 rows shows as 0).
- **Other edge cases:** `TIMING OFF`, never-executed nodes, early termination under `Limit`, Memoize and Materialize rescans, and partitions pruned at run time (`Subplans Removed`).

Sketch:

```
for n in postorder(plan):
    n.incl     = n.per_loop_time × n.loops ÷ procs(n)    # procs: number of parallel processes under a Gather, else 1
    kids       = Σ c.incl for c in children(n), excluding CTE subtrees
    if n is a CTE Scan: kids += cte_share(n)            # the scan that pulls from a CTE pays for it
                                                        # (split by rows pulled if several scans share one CTE; approximate)
    n.excl     = max(0, n.incl − kids)                  # a negative value means rounding or measurement error: flag it
    n.excl_buf = n.buffers − Σ c.buffers                # no multiplication by loops
unattributed = execution_time − root.incl − Σ trigger_time
```

Results are cross-checked against pev2 and explain.depesz.com on a shared set of reference plans.

## Predicate parsing

Filter and join conditions arrive as deparsed text such as `((status)::text = 'open'::text)`. We do not parse them with regular expressions. Instead:

1. Replace plan-only syntax (`SubPlan N`, `hashed SubPlan N`, `(InitPlan N).colX`, `$N`, `alternatives: …`) with placeholders.
2. Wrap the expression as `SELECT 1 WHERE (<expr>)` and parse it with `sqlparser-rs` using its PostgreSQL dialect. It is pure Rust and WebAssembly-friendly, and its MySQL dialect will help later.
3. Extract columns, operators, casts, function calls and constants.
4. If parsing fails, show the raw text and produce no advice for that predicate.

`libpg_query` (through `pg_query.rs`) may be added later for native builds only, where its query fingerprinting is useful for grouping queries found in logs.

## Index advisor (without requiring HypoPG)

The advisor runs in six stages:

1. **Collect evidence.** For each scan: the relation and alias; `Filter`, `Index Cond` and `Recheck Cond`; `Rows Removed by Filter`; actual rows, loops and buffers; any `Sort Key` and `Limit` above it; and, on the inner side of a nested loop, the join condition.
2. **Classify predicates:** equality, `IN`/`ANY`, range, prefix `LIKE`, containment (jsonb, arrays, full-text search → GIN), substring match (`%x%`, `ILIKE` → pg_trgm), a function of a column (→ expression index), a cast on the column side (→ fix the query, not an index), and non-sargable predicates (`OR` across columns, `<>`, `NOT`).
3. **Score the opportunity.** The plan gives us the observed selectivity for free: `s = actual_rows / (actual_rows + rows_removed_by_filter)`. A sequential scan's buffer count approximates the table size in pages. Impact is the node's share of total exclusive time. Default gates, all configurable: `s < 5%`, more than about 1,000 pages (8 MB), and impact above 10%. On the inner side of a nested loop, the benefit is multiplied by `loops`. If the plan includes a `Settings` section (for example with `random_page_cost`), it feeds into the cost model.
4. **Generate candidates.** Key columns are ordered by the ESR rule: equality first, then sort, then range. With `ORDER BY … LIMIT`, matching the sort order removes the sort and lets the scan stop early. At most one range column is used. In connected mode, a partial index is proposed when `pg_stats.most_common_freqs` shows a rare constant. `INCLUDE` columns are an optional, low-confidence addition. The operator class is chosen as needed (`text_pattern_ops`, GIN, with a warning when `pg_trgm` is required). Slow foreign-key triggers lead to an index on the referencing columns. The output is always `CREATE INDEX CONCURRENTLY`.
5. **Apply negative rules.** The advisor makes no suggestion, and shows why, when: the table is small; selectivity is above roughly 10–20%; the node is not on the hot path; a hash join's build side needs the whole table anyway; the scan already stops early under a `Limit`; there is a cast on the column side; or the predicate is an `OR` across columns. In connected mode, candidates are compared with existing indexes using the left-prefix rule. If a suitable index already exists, the advisor explains why it was probably not used (a cast, the collation, stale statistics, selectivity) instead of suggesting a duplicate.
6. **Report.** Each suggestion includes the DDL, a confidence level (high, medium or low), the evidence ("12 of 5,000,000 rows", "94% of runtime"), caveats ("not connected: existing indexes could not be checked", "write overhead on a hot table") and a verification status (unverified, estimated with HypoPG, or measured with rollback).

Quality gate: fixture scenarios marked `advice: none` are plans where a naive advisor would make a bad suggestion. The expected result for each is "no suggestion", and precision is tracked in CI.

Related work: Microsoft's AutoAdmin "what-if" indexes (Chaudhuri and Narasayya), Dexter, postgres-mcp (HypoPG with a greedy, "Anytime"-style search) and pganalyze's writing on its indexing engine.

## Connected mode and safety

- Connection settings follow libpq conventions (`PG*` environment variables, `~/.pgpass`, service files): if `psql` connects, `explainsql` should too.
- `EXPLAIN ANALYZE` really executes the statement. Every run happens inside `BEGIN … ROLLBACK` with a `statement_timeout`. Statements that do not modify data run in a `READ ONLY` transaction. Data-modifying statements require an explicit `--allow-dml`, and the tool warns that some side effects are not undone by a rollback: sequence increments, dblink calls, and functions or foreign data wrappers that reach external systems.
- The estimated plan (`EXPLAIN` without `ANALYZE`) is shown immediately. The `ANALYZE` run happens in the background and can be cancelled.
- DDL for verification is disabled by default (`--allow-ddl`), uses `SET LOCAL lock_timeout`, and asks for confirmation after showing the table size.
- Catalog reads are limited to the relations that appear in the plan.
