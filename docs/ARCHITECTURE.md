# Architecture

This document describes the design. The plan IR and the parsers are implemented (Phase 1); everything from [Metrics](#metrics-inclusive-and-exclusive-time) onwards is still planned, and this document will be updated as that code lands.

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
│  │  ├─ src/ir.rs               # the plan IR
│  │  ├─ src/pg/                 # PostgreSQL front end: normalize, json, text, raw, lower
│  │  └─ tests/                  # corpus, captured inputs, text-format cases, robustness
│  ├─ explainsql-db/             # tokio-postgres + rustls: safe executor, catalog reader, HypoPG/rollback prover
│  ├─ explainsql-tui/            # Ratatui app: state, views, keymap, theme
│  └─ explainsql/                # binary: clap CLI, mode dispatch (tui | print | pager | json)
├─ fixtures/
│  ├─ schema.sql                 # deterministic dataset
│  ├─ scenarios/<name>.sql       # one statement plus expectations (rules, advice) per scenario
│  ├─ pg/{12..18}/               # generated plans: <name>.json, <name>.txt, manifest.json
│  └─ inputs/                    # one plan in each form it arrives in: psql output, server logs
├─ fuzz/                         # cargo-fuzz target for the parsers (its own workspace; needs nightly)
├─ docs/rules/                   # one page per rule
├─ xtask/                        # gen-fixtures and check-fixtures; later an anonymizer and release helpers
└─ .github/workflows/            # ci, fixtures, release
```

Not there yet: the `metrics`, `rules`, `advisor` and `expr` modules of `core`, the contents of `explainsql-db` and `explainsql-tui` (empty placeholders for now), `docs/rules/` and the release workflow. Until then, the binary has a single developer option, `--debug-parse`, which prints what the parsers made of an input.

We use four crates and no more. Keeping `core` free of I/O is required for WebAssembly and for fast, deterministic tests; finer splits would slow down early development.

Process model: `core` is synchronous and pure. The TUI talks to the database layer over channels. The database layer runs on a background Tokio runtime and can cancel a running query.

## Plan IR

Plans are stored in an arena: nodes live in a `Vec<Node>` and refer to each other through `NodeId(u32)` indices. This avoids ownership problems with parent pointers, is cache-friendly and serializes trivially. Code that walks the tree does so iteratively, so a deep plan cannot overflow the stack.

```rust
pub struct Plan {
    pub nodes: Vec<Node>,                   // nodes[0] is the root; a node's id is its index
    pub summary: Summary,                   // planning and execution time, triggers, JIT, settings, ...
    pub source: Source,                     // JSON or text, and the wrappers removed (psql table, log entry, ...)
    pub warnings: Vec<Warning>,             // problems found while parsing; the plan is usable despite them
}

pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub node_type: String,                  // PostgreSQL's name: "Seq Scan", "Hash Join", "Aggregate", ...
    pub relationship: Option<Relationship>, // Outer | Inner | Member | InitPlan | SubPlan | Subquery
    pub subplan_name: Option<String>,       // "InitPlan 1", "SubPlan 2", "CTE totals"
    pub join_type: Option<String>,          // likewise strategy, operation, relation, index, alias, ...
    pub estimates: Option<Estimates>,       // cost, rows, width; None with COSTS OFF
    pub actuals: Option<Actuals>,           // per-loop time and rows, plus loops; None without ANALYZE
    pub buffers: Option<Buffers>,           // TOTALS across loops, not per loop (likewise io_timings, wal)
    pub predicates: Vec<Predicate>,         // Index Cond, Hash Cond, Filter, ... as raw text
    pub workers: Vec<Worker>,               // per-worker figures of parallel nodes
    pub extra: BTreeMap<String, serde_json::Value>, // every other property, under its JSON name
    // ...plus output columns, sort and group keys, rows removed by filters, workers planned and launched
}
```

- **Typed fields cover what later phases rely on; everything else is kept.** Other properties stay in `extra` under their PostgreSQL JSON names (`Heap Fetches`, `Sort Method`, `Hash Buckets`, ...), and the statement-level sections (planning, triggers, JIT, serialization) keep their unfamiliar keys the same way. Nothing in the input is lost, and properties added by future server versions show up without code changes.
- **Node types are strings, not an enum.** Extensions and forks add their own (Citus and TimescaleDB custom scans, Greenplum's `Motion`), and code that cares matches on the names it knows.
- **Absent and zero mean the same.** The text format leaves out zero counters and false flags, so the IR does too, whichever format a plan came from: all-zero buffers become `None`, a zero `Subplans Removed` is dropped, and so on. This is what lets the JSON and text forms of a plan lower to identical IR.
- **Derived metrics live beside the IR.** Inclusive and exclusive time, shares of the total and misestimate factors will be computed by the metrics engine (Phase 2) and indexed by `NodeId`. Every derived value will carry its provenance (`Measured`, `Estimated`, `Derived` or `Unknown`), so the UI never presents a guess as a measurement.

The IR uses PostgreSQL's vocabulary, since PostgreSQL is the only engine for now, but its structure (arena, estimates, actuals, predicates, `extra`) is engine-neutral. A future MySQL front end would lower into the same IR, as the `pg` module does.

## Parsing pipeline

```
input ─▶ normalize() ─┬─▶ json::parse() ─┬─▶ raw tree ─▶ lower() ─▶ Plan
                      └─▶ text::parse() ─┘
```

- **JSON is the primary format and the source of truth.** When connected, we will always request `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON)`, with the options adjusted to the server version.
- **The text format is supported from v0.1.** It is the default in psql and in `auto_explain`, and most plans shared in issues and chats are text. Accepting only JSON would turn away a large share of users on their first try.
- **`normalize()`** removes what surrounds a plan and tells JSON (input starting with `[` or `{`) from text. It handles psql's aligned output (ASCII and Unicode line styles, borders 0–2, `+` and `↵` continuation marks, the `(N rows)` footer) as well as its wrapped, expanded and CSV formats; `auto_explain` entries in stderr logs (whatever the `log_line_prefix`), `jsonlog` and `csvlog`, keeping the logged query text; result cells copied in double quotes, as GUI clients such as pgAdmin copy them; Markdown code fences; prompts and other text before a plan; shared indentation; and byte order marks, CRLF line endings and non-breaking spaces. Wrappers can nest (a fenced log excerpt), and each one removed is recorded in `Plan::source`.
- **Both parsers build the same raw tree:** nodes holding their properties under PostgreSQL's JSON names. The JSON parser reads it off directly. The text parser translates each line into those names: `Buffers: shared hit=5 read=2` becomes `Shared Hit Blocks` and `Shared Read Blocks`, and `Sort Method: quicksort  Memory: 25kB` becomes `Sort Method`, `Sort Space Type` and `Sort Space Used`. One lowering step then serves both formats, and comparing them is direct.
- **The text parser is hand-written**, with no regular expressions and no parser generator. An indentation stack follows the layout rules of PostgreSQL's `explain.c`: a node's properties start two columns to the right of its name, a child's `->` arrow sits in its parent's property column, and an `InitPlan`, `SubPlan` or `CTE` label sits in the property column with its node two columns further in. Lines after the tree that start in column 0 belong to the statement (`Planning:`, triggers, `JIT:`, `Settings:`, `Execution Time`, ...). Text plans do not print relationships; they are inferred from the parent's type and the child's position.
- **`lower()`** builds the typed IR, absorbing version drift (below) and the differences that only reflect how a plan was printed.
- **Never fail hard.** Unfamiliar properties are kept in `extra`. In text plans they also produce a warning, and a line that cannot be read at all is kept verbatim in `extra["Unparsed Lines"]`. A truncated JSON plan is closed after its last complete value, and a truncated text plan keeps the nodes before the cut. Input holding several plans, such as a before-and-after pair, yields the first one and a warning. Only input with no plan in it is rejected, with one exception: serde_json parses recursively, so JSON nested deeper than 512 levels (255 plan levels) is refused rather than risking the stack. Text plans have no depth limit.
- **Not supported:** the YAML and XML formats.

### Version and fork drift (examples)

- PostgreSQL 13: buffers used during planning; the `WAL` option.
- PostgreSQL 14: the `Memoize` node.
- PostgreSQL 16: the `GENERIC_PLAN` option.
- PostgreSQL 17: the `SERIALIZE` and `MEMORY` options; I/O timings split into shared and local; subplan outputs shown as `(InitPlan 1).col1` instead of `$0`.
- PostgreSQL 18: `BUFFERS` is on by default with `ANALYZE`; actual row counts are always printed with two decimals (`rows=10.00`, and `"Actual Rows": 10.00` in JSON, so parsers must read them as floats); `Index Searches`; `Disabled: true` on nodes the planner had to use despite an `enable_*` setting. Older versions add a huge `disable_cost` of 1e10 instead, which a naive tool mistakes for the most expensive node.
- Extensions and forks: `Custom Scan` nodes (Citus, TimescaleDB), `Motion` nodes (Greenplum). Unknown node types are rendered generically, never rejected.

The parsers handle all of the above for PostgreSQL 12–18, and the corpus covers every version. The corpus also surfaced drift that is easy to miss: the source of an `INSERT`, `UPDATE` or `DELETE` is a `Member` of `ModifyTable` up to PostgreSQL 13 and its `Outer` child from 14; JIT generation time becomes an object with a separate `Deform` part in 17; and PostgreSQL 12 labels the leader's JIT figures as worker −1. `lower()` maps each of these, like the renamed I/O timing keys, to a single form.

### Testing the parsers

- **Fixture corpus** (`tests/corpus.rs`): all 882 generated plans parse without a single warning, and for each of the 441 scenario–version pairs the JSON and text forms lower to the same IR: tree shape, node types and relationships, every typed property, estimates, actual rows and loops, and everything in `extra`. The two forms come from separate executions (see [fixtures/README.md](../fixtures/README.md)), so timings, buffer counts and per-worker figures are not compared, and two kinds of values are excluded on principle: memory figures of nodes below a `Gather`, which depend on how much of the work the leader did, and estimates of data-modifying statements, because rolled-back writes still grow the table and the planner scales its estimates by the table's current size. A guard test checks that the comparison does notice changed values.
- **Captured inputs** (`tests/inputs.rs`, [`fixtures/inputs/`](../fixtures/inputs)): one query's plan, captured from a real server in every form it arrives in (psql's aligned, Unicode, bordered, wrapped, expanded and CSV output, in text and JSON; `auto_explain` entries in stderr, `jsonlog` and `csvlog` logs), must yield the same tree as the plain text plan. Generated variants add cells copied from GUI clients, Markdown fences, CRLF, prompts, indentation, non-breaking spaces, truncation, several plans in one input, and inputs that must be rejected.
- **Constructs the corpus does not reach** (`tests/text_format.rs`, `tests/unknown_properties.rs`): other join, aggregate and set-operation variants, quoted identifiers, foreign and custom scans, compound property lines, the statement summary, and unfamiliar properties at every level of both formats.
- **Robustness** (`tests/robustness.rs`): thousands of truncated and mutated corpus plans, and pathological input (1,500 levels of indentation, a 20,000-child `Append`, JSON nested 100,000 levels deep, malformed fragments of every construct), must never cause a panic.
- **Fuzzing** (`fuzz/`): `cargo +nightly fuzz run parse`, seeded with the corpus and the captured inputs. The Phase 1 exit criterion is one hour on three workers without a crash.
- **Snapshots:** the IR and the rendered output will be snapshot-tested with `insta` once there is rendered output to check (Phase 2).

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
