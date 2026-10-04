# Architecture

This document describes the design. The plan IR and the parsers (Phase 1), and the metrics engine, the rules and the static report (Phase 2) are implemented; everything from [Predicate parsing](#predicate-parsing) onwards is still planned, and this document will be updated as that code lands.

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
│  │  ├─ src/metrics.rs          # inclusive and exclusive time and buffers, misestimates
│  │  ├─ src/rules/              # one file per rule, plus a small predicate reader
│  │  ├─ src/analysis.rs         # metrics + findings + the one-sentence verdict
│  │  ├─ src/report.rs           # static reports: text, Markdown, JSON
│  │  └─ tests/                  # corpus, inputs, metrics, rules, report snapshots, robustness
│  ├─ explainsql-db/             # tokio-postgres + rustls: safe executor, catalog reader, HypoPG/rollback prover
│  ├─ explainsql-tui/            # Ratatui app: state, views, keymap, theme
│  └─ explainsql/                # binary: clap CLI, mode dispatch (tui | print | pager | json)
├─ fixtures/
│  ├─ schema.sql                 # deterministic dataset
│  ├─ scenarios/<name>.sql       # one statement plus expectations (rules, advice) per scenario
│  ├─ pg/{12..18}/               # generated plans: <name>.json, <name>.txt, manifest.json
│  └─ inputs/                    # one plan in each form it arrives in: psql output, server logs
├─ fuzz/                         # cargo-fuzz target for the parsers (its own workspace; needs nightly)
├─ tools/cross-check/            # compares exclusive times with pev2 and explain.depesz.com
├─ docs/rules/                   # one page per rule
├─ xtask/                        # gen-fixtures and check-fixtures; later an anonymizer and release helpers
└─ .github/workflows/            # ci, fixtures, release
```

Not there yet: the `advisor` and `expr` modules of `core`, the contents of `explainsql-db` (an empty placeholder for now), `docs/rules/` and the release workflow. In a terminal the binary opens the viewer; elsewhere, or with `--print`, it prints a report (`--format text|md|json`). `--debug-parse` shows what the parsers made of an input.

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
- **Derived metrics live beside the IR.** The metrics engine computes inclusive and exclusive time, shares of the total and misestimate factors into a separate structure indexed by `NodeId`. A figure the plan cannot support is `None`: without `ANALYZE` or with `TIMING OFF` there are no times, and nothing is filled in from estimates, so the UI never presents a guess as a measurement.

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
- **Robustness** (`tests/robustness.rs`): thousands of truncated and mutated corpus plans, and pathological input (1,500 levels of indentation, a 20,000-child `Append`, JSON nested 100,000 levels deep, malformed fragments of every construct), must never cause a panic in the parsers, the analysis or the reports.
- **Fuzzing** (`fuzz/`): `cargo +nightly fuzz run parse`, seeded with the corpus and the captured inputs; a second target, `analyze`, also runs the analysis and the reports. The Phase 1 exit run of `parse` lasted one hour on three workers: 2.9 million inputs and no crash, timeout or memory blow-up.
- **Snapshots** of the rendered reports cover the parsers' output end to end; see [Testing the metrics and the rules](#testing-the-metrics-and-the-rules).

## Metrics: inclusive and exclusive time

Getting per-node numbers right is harder than it looks, and everything else is built on it. `metrics::compute` works on the IR alone.

- **Times and rows are per-loop averages; buffers are totals across loops.** Time is multiplied by `loops`; buffers never are.
- **Parallel query.** Below a `Gather` or `Gather Merge`, `loops` counts the processes that ran a node side by side, so `time × loops` is CPU time, not wall-clock time, and subtracting it makes the Gather's own time negative. The engine counts the processes as the loops of the Gather's child per loop of the Gather (workers plus the leader, when it takes part). It divides by that number for wall-clock time and keeps the undivided figure as CPU time.
- **CTEs.** A CTE runs as the `CTE Scan`s reading it pull rows, so its time is already inside those scans. It is not subtracted from the node it is listed under. The scans subtract it instead, in proportion to their own time: the scan that pulls rows first computes them, and later ones read them from the CTE's store.
- **InitPlans.** An InitPlan runs when its result is first needed, inside the node that needs it. That node subtracts it, rather than the node the InitPlan is listed under. The engine finds it by the reference to the result: `$0` before PostgreSQL 17, `(InitPlan 1).col1` from 17. When several nodes refer to it, the first in execution order (post-order) takes it.
- **SubPlans** run from the expressions of the node they are listed under, which subtracts them like any child.
- **Rounding.** Times are printed per loop with 0.001 ms resolution, so over 20,000 loops a figure can be off by 10 ms, and a parent can show less time than its children together. Within that tolerance, the engine moves the gap to the least precise figures (those with the most loops), never below what their own children need. Exclusive times then add up to the tree's time. A node whose children exceed it by more than rounding explains is flagged as inconsistent; none is in the corpus.
- **Time outside the tree.** Triggers (including foreign-key checks, which can dominate a slow `DELETE`), `SERIALIZE` and executor startup are not part of any node. They are reported for the statement, along with an unattributed remainder: execution time minus the tree, triggers and serialization. JIT compilation falls partly inside node times and partly outside, so it is reported on its own.
- **Misestimates** compare actual and estimated rows per loop, each counted as at least one row. A node that a `Limit`, a semi or anti join, a merge join or a subquery can stop early is marked, so that returning fewer rows than estimated is not mistaken for a bad estimate.
- **Never-executed nodes** count as zero; with `TIMING OFF` or without `ANALYZE`, times are `None` and hotspots are ranked by buffers.

The results agree with pev2 and explain.depesz.com, compared node by node on 24 reference plans from PostgreSQL 13, 16 and 18 (see [tools/cross-check](../tools/cross-check/README.md)). Every node is within 5%, except in the Memoize plan above. There, both tools clamp the negative rounding gap to zero, so their exclusive times add up to more than the statement took.

## Rules and reports

- **Rules** (`rules/`) read the IR and the metrics and return findings: the rule, the node, a severity from the share of the runtime involved, the evidence, and an action. Each rule is one file with its thresholds as constants, and each documents when it stays silent. The catalog is [rules.md](rules.md).
- **Conditions** are read by a small predicate reader. It is enough to tell which columns a filter compares with what, whether it wraps them in a cast or a function, and whether it ORs conditions on different columns. The full expression parser comes with the advisor (below).
- **The verdict** is one sentence: the statement's time, where most of it went, and the finding about that node, if any. For example: `11.9 ms. 100% of it in Seq Scan on orders, which reads 200,000 rows to keep 10.`
- **Reports** (`report.rs`) come in three formats:
  - text for terminals: the verdict, statement figures, the plan tree with exclusive time, bars and misestimate marks, and the findings;
  - Markdown for issues and pull requests;
  - JSON with the plan, the metrics and the findings, for other programs.

### Testing the metrics and the rules

- **Metrics invariants over the corpus** (`tests/metrics.rs`), across 882 plans:
  - no node is inconsistent;
  - exclusive times add up to the tree's time;
  - the tree never exceeds the execution time;
  - shares add up to at most 100%.

  Unit tests cover each exception above on small plans.
- **Rules against the scenarios** (`tests/rules.rs`): each scenario's header lists the rules its plan triggers, and no other rule may fire, in either format on any version. A rule marked `?` may fire on some versions only, where the planner's estimates differ. Scenarios without rules, such as most of the traps for naive advisors, expect silence. A second test checks that the actions name the right columns and remedies.
- **Snapshots** (`tests/report.rs`, with `insta`): the text report of 24 reference plans and a Markdown report. A change in the metrics, the rules or the layout shows up as a reviewable diff.
- **The binary** (`crates/explainsql/tests/cli.rs`): formats, standard input, exit codes, and a reader that closes the pipe early.

## The viewer

`explainsql-tui` shows a plan and its analysis. It depends only on `core` and Ratatui (0.29, the last release that builds with Rust 1.85), with Crossterm as the backend.

- **State apart from drawing.** `app.rs` holds the state: the visible rows, the selection, folds, search, focus and view modes. It turns keys into changes, with no terminal involved, so navigation is unit-tested directly. `ui.rs` draws a frame from that state, and `lib.rs` runs the loop: draw, wait for a key or a resize, handle it. Nothing is redrawn while nothing happens.
- **Layout.** The verdict and statement figures sit on top. Below them are the plan tree (share, time, bar, node, rows, estimate with ▲▼ marks, buffers), the details of the selected node, the findings and a status line. From 110 columns the details sit beside the tree; below that they go under it, and the tree takes only the rows it needs. Columns are dropped (buffers, then the bar, then the estimate) before the node names get shorter than 30 characters, so 80×24 stays usable.
- **Virtualized tree.** Only the rows on screen are built and drawn. Node names and column widths are computed once, when the viewer opens. A frame of a 5,000-node plan takes about 0.3 ms in a release build.
- **Folding.** Any node can be folded. Runs of four or more similar leaves are folded into one row from the start, such as the scans of a thousand partitions. Leaves are similar when they have the same type, and the same relation and conditions once numbers are blanked out. The folded row adds up their time, rows and buffers.
- **Views.** `x` shows time including children, `w` shows CPU time summed over parallel processes, and `b` shares by buffers instead of time. Including children, CPU time is summed over the subtree, because a `Gather`'s own figures cover only the leader.
- **Findings.** A marker in the tree shows which nodes have findings. The findings list is browsable, and Enter jumps to the node, opening whatever folds hide it. Number keys jump to the hotspots.
- **Colors.** True color, then 256 colors, then 16, depending on `COLORTERM` and `TERM`. With `NO_COLOR` it falls back to bold, dim and reverse video. `--theme light` adapts the palette to light backgrounds. Severities and misestimates always carry a word or a symbol as well as a color.
- **Input.** Keys come from the terminal even when the plan arrived on standard input: Crossterm opens `/dev/tty` on Unix and the console input on Windows.
- **Pager mode.** `--pager` reads what psql sends to its pager. A plan opens in the viewer. Anything else goes to `$EXPLAINSQL_PAGER`, `$PAGER` or `less -S`, never back to `explainsql`, and is printed directly when none of them runs. When the output is not a terminal, everything passes through unchanged.
- **Tests.** `tests/render.rs` draws frames with Ratatui's `TestBackend` and compares them with snapshots: five reference plans at 120×40 and 80×24, help, search, the findings and the view modes. A 5,000-node plan that cannot be folded must draw a frame in under 16 ms in release builds (200 ms in debug builds).

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
