# Architecture

This document describes how ExplainSQL is built and why it is built that way: the choice of language, the crates, the plan IR, the parsers, the metrics engine, the rules and the advisor, connected mode and each analysis built on it, and the release pipeline. Everything described here is implemented. For how to use each feature, see the [user guide](guide.md); for how to work on the code, see [contributing](contributing.md).

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
│  │  ├─ src/pg/                 # PostgreSQL front end: normalize, json, text, raw, lower; log entries
│  │  ├─ src/metrics.rs          # inclusive and exclusive time and buffers, misestimates
│  │  ├─ src/expr.rs             # reads the conditions printed in plans
│  │  ├─ src/rules/              # one file per rule
│  │  ├─ src/advisor/            # index candidates, rewrites, and why no index
│  │  ├─ src/catalog.rs          # what the database says about the plan's tables
│  │  ├─ src/check.rs            # the CI gate: findings, locked plans, explainsql.lock
│  │  ├─ src/compare.rs          # before and after a change: pages first, then time
│  │  ├─ src/memory.rs           # the work_mem a spill needs, and what it may take
│  │  ├─ src/diff.rs             # two plans of a statement, node by node
│  │  ├─ src/scenario.rs         # the planner settings explainsql may plan under
│  │  ├─ src/fingerprint.rs      # the same scan or join in another plan; plan shapes
│  │  ├─ src/counterfactual.rs   # why the planner chose its plan: questions and answers
│  │  ├─ src/params.rs           # statements with parameters: values to try, generic and custom plans
│  │  ├─ src/locks.rs            # the locks a statement takes: fast path, partitions, conflicts, waits
│  │  ├─ src/writes.rs           # what a write costs: HOT updates, the indexes that block them, WAL
│  │  ├─ src/top.rs              # pg_stat_statements rows: what can be planned, and why not
│  │  ├─ src/anonymize.rs        # names and values replaced, consistently, in JSON and text plans
│  │  ├─ src/timeline.rs         # plans over time from server logs: statements, plan changes, tags
│  │  ├─ src/requests.rs         # requests from statement logs: grouping, loops (N+1), the batched statement
│  │  ├─ src/analysis.rs         # metrics + findings + the one-sentence verdict
│  │  ├─ src/report.rs           # static reports: text, Markdown, JSON
│  │  └─ tests/                  # corpus, inputs, metrics, rules, report snapshots, robustness
│  ├─ explainsql-db/             # tokio-postgres + rustls: safe executor, prepared statements, catalog reader, locks, writes, HypoPG/rollback prover
│  ├─ explainsql-tui/            # Ratatui app: state, views, keymap, theme, icicle, the top list
│  └─ explainsql/                # binary: clap CLI, mode dispatch (tui | print | pager | json), connected mode, check, logs, top, requests
├─ fixtures/
│  ├─ schema.sql                 # deterministic dataset
│  ├─ scenarios/<name>.sql       # one statement plus expectations (rules, advice) per scenario
│  ├─ pg/{12..18}/               # generated plans: <name>.json, <name>.txt, manifest.json
│  ├─ inputs/                    # one plan in each form it arrives in: psql output, server logs
│  ├─ logs/                      # a session's auto_explain entries in stderr, csvlog and jsonlog
│  └─ requests/                  # an application's requests, as statement logging writes them
├─ fuzz/                         # cargo-fuzz target for the parsers (its own workspace; needs nightly)
├─ tools/cross-check/            # compares exclusive times with pev2 and explain.depesz.com
├─ action/                       # the GitHub Action's scripts (action.yml is at the root)
├─ docs/                         # the documentation site (mdBook): getting started, guide chapters, reference, rule catalog, design documents
│  ├─ rules/ES001.md …           # one page per rule, with an example from the corpus
│  └─ demo.svg                   # the README's demo, drawn from xtask/demo/recording.json
├─ install/                      # install.sh, install.ps1, packaging and smoke tests for releases
├─ xtask/                        # fixtures, rule pages, link check, demo; demo/: its query and recording
└─ .github/workflows/            # ci, docs, release, fixtures
```

In a terminal the binary opens the viewer; elsewhere, or with `--print`, it prints a report (`--format text|md|json`). `--debug-parse` shows what the parsers made of an input.

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

- **JSON is the primary format and the source of truth.** When connected, ExplainSQL always requests `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON)` (`VERBOSE, SETTINGS, FORMAT JSON` for the estimated plan), with `SETTINGS` from PostgreSQL 12 and `WAL` added for statements that write from 13.
- **The text format is supported from v0.1.** It is the default in psql and in `auto_explain`, and most plans shared in issues and chats are text. Accepting only JSON would turn away a large share of users on their first try.
- **`normalize()`** removes what surrounds a plan and tells JSON (input starting with `[` or `{`) from text. It handles psql's aligned output (ASCII and Unicode line styles, borders 0–2, `+` and `↵` continuation marks, the `(N rows)` footer) as well as its wrapped, expanded and CSV formats; `auto_explain` entries in stderr logs (whatever the `log_line_prefix`), `jsonlog` and `csvlog`, keeping the logged query text; result cells copied in double quotes, as GUI clients such as pgAdmin copy them; Markdown code fences; prompts and other text before a plan; shared indentation; and byte order marks, CRLF line endings and non-breaking spaces. Wrappers can nest (a fenced log excerpt), and each one removed is recorded in `Plan::source`.
- **Both parsers build the same raw tree:** nodes holding their properties under PostgreSQL's JSON names. The JSON parser reads it off directly. The text parser translates each line into those names: `Buffers: shared hit=5 read=2` becomes `Shared Hit Blocks` and `Shared Read Blocks`, and `Sort Method: quicksort  Memory: 25kB` becomes `Sort Method`, `Sort Space Type` and `Sort Space Used`. One lowering step then serves both formats, and comparing them is direct.
- **The text parser is hand-written**, with no regular expressions and no parser generator. An indentation stack follows the layout rules of PostgreSQL's `explain.c`: a node's properties start two columns to the right of its name, a child's `->` arrow sits in its parent's property column, and an `InitPlan`, `SubPlan` or `CTE` label sits in the property column with its node two columns further in. Lines after the tree that start in column 0 belong to the statement (`Planning:`, triggers, `JIT:`, `Settings:`, `Execution Time`, ...). Text plans do not print relationships; they are inferred from the parent's type and the child's position.
- **`lower()`** builds the typed IR, absorbing version drift (below) and the differences that only reflect how a plan was printed.
- **Never fail hard.** Unfamiliar properties are kept in `extra`. In text plans they also produce a warning, and a line that cannot be read at all is kept verbatim in `extra["Unparsed Lines"]`. A truncated JSON plan is closed after its last complete value, and a truncated text plan keeps the nodes before the cut. Input holding several plans, such as a before-and-after pair, yields the first one and a warning. Only input with no plan in it is rejected, with one exception: serde_json parses recursively, so JSON nested deeper than 512 levels (255 plan levels) is refused rather than risking the stack. Text plans have no depth limit.
- **Several plans.** `parse()` reads the first plan of the input and warns of others; `parse_all()` reads them all, in order. `normalize_all()` keeps every Markdown fence, every auto_explain entry of a log (each with its query text) and every psql result; then the JSON parser reads each plan of an array and each JSON value that follows, and the text parser starts a new plan at each root line in column 0. Text between two plans that belongs to neither, such as `After:`, is left out with a warning on the plan it precedes. For an input with one plan, `parse_all()` returns what `parse()` does; the corpus and the input fixtures check it.
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

- **Fixture corpus** (`tests/corpus.rs`): all 882 generated plans parse without a single warning, and for each of the 441 scenario–version pairs the JSON and text forms lower to the same IR: tree shape, node types and relationships, every typed property, estimates, actual rows and loops, and everything in `extra`. The two forms come from separate executions (see [fixtures/README.md](https://github.com/onplt/explain-sql/blob/HEAD/fixtures/README.md)), so timings, buffer counts and per-worker figures are not compared, and two kinds of values are excluded on principle: memory figures of nodes below a `Gather`, which depend on how much of the work the leader did, and estimates of data-modifying statements, because rolled-back writes still grow the table and the planner scales its estimates by the table's current size. A guard test checks that the comparison does notice changed values.
- **Captured inputs** (`tests/inputs.rs`, [`fixtures/inputs/`](https://github.com/onplt/explain-sql/tree/HEAD/fixtures/inputs)): one query's plan, captured from a real server in every form it arrives in (psql's aligned, Unicode, bordered, wrapped, expanded and CSV output, in text and JSON; `auto_explain` entries in stderr, `jsonlog` and `csvlog` logs), must yield the same tree as the plain text plan. Generated variants add cells copied from GUI clients, Markdown fences, CRLF, prompts, indentation, non-breaking spaces, truncation, several plans in one input, and inputs that must be rejected.
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
- **I/O time** (`track_io_timing`) is subtracted like buffers: each node keeps what it read and wrote itself. For the statement, the root's figures, which include every node and every process, are split into reads of pages outside shared buffers, writes, and temporary files, and compared with the time of every process: the nodes' own CPU time, summed. Compared with the leader's wall-clock time alone, a parallel scan that waits for reads in three processes would take more than all of it.

The results agree with pev2 and explain.depesz.com, compared node by node on 24 reference plans from PostgreSQL 13, 16 and 18 (see [tools/cross-check](https://github.com/onplt/explain-sql/blob/HEAD/tools/cross-check/README.md)). Every node is within 5%, except in the Memoize plan above. There, both tools clamp the negative rounding gap to zero, so their exclusive times add up to more than the statement took.

## Rules and reports

- **Rules** (`rules/`) read the IR and the metrics and return findings: the rule, the node, a severity from the share of the runtime involved, the evidence, and an action. Each rule is one file with its thresholds as constants, and each documents when it stays silent. The catalog is [rules.md](rules.md).
- **Conditions** are read by the predicate reader in `expr.rs` (see [Predicate parsing](#predicate-parsing)): enough to tell which columns a filter compares with what, whether it wraps them in a cast or a function, and whether it ORs conditions on different columns.
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
- **Findings and advice.** A marker in the tree shows which nodes have findings. The panel under the tree lists the findings (`f`) or the advice (`i`). Both lists are browsable, and Enter jumps to the node, opening whatever folds hide it. `c` copies the selected `CREATE INDEX` with the OSC 52 escape sequence, which works over SSH and inside tmux. Number keys jump to the hotspots.
- **Colors.** True color, then 256 colors, then 16, depending on `COLORTERM` and `TERM`. With `NO_COLOR` it falls back to bold, dim and reverse video. `--theme light` adapts the palette to light backgrounds. Severities and misestimates always carry a word or a symbol as well as a color.
- **Input.** Keys come from the terminal even when the plan arrived on standard input: Crossterm opens `/dev/tty` on Unix and the console input on Windows.
- **Pager mode.** `--pager` reads what psql sends to its pager. A plan opens in the viewer. Anything else goes to `$EXPLAINSQL_PAGER`, `$PAGER` or `less -S`, never back to `explainsql`, and is printed directly when none of them runs. When the output is not a terminal, everything passes through unchanged.
- **Tests.** `tests/render.rs` draws frames with Ratatui's `TestBackend` and compares them with snapshots: five reference plans at 120×40 and 80×24, help, search, the findings and the view modes. A 5,000-node plan that cannot be folded must draw a frame in under 16 ms in release builds (200 ms in debug builds).

### The icicle view

`icicle.rs` draws the plan as nested boxes: the root on top, each node under its parent, as wide as the time spent in it and below it. The weight is CPU time when the plan was timed, so that the workers of a parallel plan add up under the node that gathers them, and estimated cost otherwise, a node's own cost being its cost less its children's. InitPlans, SubPlans and CTEs sit under the node they are listed under; trigger time, outside the tree, goes in the title. Widths are whole columns: a subtree too narrow for one is folded into its parent and drawn as `…`, and zooming on a box redraws it at the full width, which opens those folds. The view shares the tree's selection, details, search and hotspots, so `F` switches between the two on the same node, and `tests/render.rs` snapshots it.

## Predicate parsing

Filter and join conditions arrive as deparsed text such as `((status)::text = 'open'::text)`. `expr.rs` reads them with a small hand-written reader, not regular expressions:

- It splits conditions at the top-level `AND` and `OR`, outside parentheses, brackets and quotes, and finds the comparison operator of each part.
- It tells a column from a value, a cast of a column (`(customer_id)::text`) and a function of one (`date_trunc('day', created_at)`).
- It treats plan-only syntax as values: `$1`, `(InitPlan 1).col1`, `ANY ('{…}')` and `ARRAY[…]`.

The rules and the advisor share it. The reader only has to understand what PostgreSQL's deparser prints, a narrow and regular dialect, and it has been run over every condition in the corpus.

We considered `sqlparser-rs`, which would mean wrapping each condition as `SELECT 1 WHERE (…)` and first replacing plan-only syntax. The hand-written reader won for three reasons: it adds no dependency, it stays WebAssembly-friendly, and it never rejects deparse-only forms such as `~~` for `LIKE`. A condition it cannot read produces no advice. `libpg_query` (through `pg_query.rs`) may be added later for native builds only, where its query fingerprinting is useful for grouping queries found in logs.

## Index advisor (without requiring HypoPG)

`advisor/` turns the analysis into suggestions. Precision comes first: a wrong `CREATE INDEX` costs the reader more than a missing one.

- **Where candidates come from.** The rules have already decided that a scan is worth an index, with their gates on selectivity (under 5%), table size (1,000 pages or more), share of the runtime (10% or more) and early stops. The advisor takes their findings:
  - ES001, a selective sequential scan: index the filtered columns, or for the inner side of a nested loop, the join key;
  - ES005, a nested loop: index the inner side's join key;
  - ES006, an index scan that filters: a composite index that also covers the filtered columns;
  - ES009, a foreign-key trigger: index the constraint's referencing columns.

  Three patterns no rule covers are added, with the same gates:
  - `ORDER BY … LIMIT` sorting a whole table with a top-N heapsort: an index in the sort order;
  - selective scans of every partition of a table, each too small for ES001 but large together: one index on the partitioned table;
  - a correlated subquery (`SubPlan`) that rescans a table for every outer row: index the column it compares with the outer row.
- **Keys** (`keys.rs`). Each condition is classified:
  - equality: `=`, `IN`/`= ANY`, `IS NULL`, ORs on one column;
  - range;
  - prefix `LIKE`: b-tree with `text_pattern_ops`;
  - substring `LIKE`/`ILIKE`: GIN with `gin_trgm_ops`;
  - containment (`@>`, `&&`, `@@`, …): GIN;
  - a cast or function of the column;
  - an `OR` across columns.

  Columns follow the ESR rule: equality first, then the sort order, then at most one range. A comparison with another table's column is a join key, which counts as equality. Plans do not list an index's columns. When an index scan under a `Limit` was chosen for its order, the sort column is read from PostgreSQL's default index name (`orders_created_at_idx`), and the suggestion says so.
- **Rewrites.** A condition that wraps the column in a cast or a function gets a rewrite instead of an index, since no index on the column can serve it.
- **Explanations.** A sequential scan that takes 10% or more of the runtime and gets no suggestion is explained, in this order:
  - it has no filter, so the query needs every row;
  - a `Limit` or semi join stops it early;
  - its filter ORs different columns;
  - the table is small;
  - it keeps too many rows.
- **Merging.** The same index found twice, or an index whose columns start another candidate's, is reported once, with the evidence of both.
- **Output.** Each suggestion has:
  - its DDL, always `CREATE INDEX CONCURRENTLY` except on partitioned tables, where PostgreSQL does not support it (the caveat says how to build it without blocking writes);
  - a confidence: high, lowered to medium for an inferred column, an operator class that depends on the collation, `pg_trgm`, or a comparison with a run-time value;
  - the evidence and the caveats;
  - a verification status: unverified, estimated with HypoPG, or measured with rollback.

  Without a connection every suggestion says that existing indexes, the write load and the statistics were not checked. A foreign-key suggestion names the constraint and the query that lists its columns, since the plan does not show them.

Quality gate (`tests/advisor.rs`): every scenario says what the advisor must conclude: `advice: none` (a trap for naive advisors), `advice: rewrite`, or `advice: index` with each expected index in an `index:` line. On every version and in both formats, a trap gets no suggestion, and an index scenario gets exactly its indexes. A scenario without an `advice` line must get no suggestion either, so every suggestion the corpus produces has been reviewed.

In connected mode the catalog refines the advice (see "Connected mode and safety" below). Partial indexes for rare constants in `pg_stats.most_common_freqs` and `INCLUDE` columns are left for later.

Related work: Microsoft's AutoAdmin "what-if" indexes (Chaudhuri and Narasayya), Dexter, postgres-mcp (HypoPG with a greedy, "Anytime"-style search) and pganalyze's writing on its indexing engine.

## Connected mode and safety

`explainsql -d "$DATABASE_URL" -f slow.sql` (or `-c "SELECT …"`) runs the query itself. `explainsql-db` holds the connection on a small Tokio runtime behind a blocking API. The binary runs it on a worker thread that talks to the viewer over channels.

- **Connection settings follow libpq** (`conn.rs`): if `psql` connects, `explainsql` does too. In order of precedence:
  1. what `-d` gives: a URL, `key=value` settings or a database name;
  2. the service file (`PGSERVICE`, `~/.pg_service.conf`, then the system file);
  3. the `PG*` environment variables;
  4. the defaults: the Unix socket, or localhost, port 5432 and the user's name.

  The password comes from `~/.pgpass` (`%APPDATA%\postgresql\pgpass.conf` on Windows). Like libpq, explainsql ignores the file when others can read it. TLS uses rustls (`tls.rs`) with libpq's meanings:
  - `prefer` and `require` encrypt without checking the certificate;
  - `verify-ca` checks the chain against `sslrootcert` or the system store;
  - `verify-full` also checks the host name.
- **Every run is rolled back** (`exec.rs`). Each `EXPLAIN` happens inside `BEGIN … ROLLBACK`, with `SET LOCAL statement_timeout` (`--timeout`, 30 s by default). `ROLLBACK` runs whatever happened before it, and the code has no path that commits.
- **The estimated plan comes first.** It shows at once, and it tells what the statement does. A `ModifyTable` node (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, also inside a `WITH`) or a `LockRows` node (`FOR UPDATE`) means the statement writes. Such statements run under `EXPLAIN ANALYZE` only with `--allow-dml`, whose help warns that sequences, dblink calls and other effects outside the database are not undone. Every other statement runs in a `READ ONLY` transaction, where even a function that writes fails.
- **One statement, of a kind EXPLAIN takes.** Statements go through the extended query protocol, which refuses several statements in one string. Anything that does not start with `SELECT`, `WITH`, `VALUES`, `TABLE`, `INSERT`, `UPDATE`, `DELETE` or `MERGE` is refused before anything runs. That covers DDL, `CREATE TABLE AS` and a pasted `EXPLAIN`.
- **In the viewer**, `EXPLAIN ANALYZE` runs in the background while the estimated plan is shown. The status line counts the seconds, and `Esc` cancels the run through PostgreSQL's cancel request. `r` runs the statement again. `e` opens it in `$VISUAL` or `$EDITOR`, then shows the estimated plan of the edited statement and runs it.
- **Catalog reads** (`catalog.rs`) cover only what the advisor needs. They run in a read-only transaction:
  - for the tables in the plan, in the advice and behind the foreign keys it names: the size, the indexes (with their key columns and validity), the column collations and `n_distinct`, and the last analyze;
  - the columns of those foreign keys;
  - the installed extensions.

  `advisor::refine` then:
  - turns a candidate that an existing valid index already covers (by the left-prefix rule) into an explanation of why the planner probably did not use it;
  - turns a foreign-key suggestion into a `CREATE INDEX` on its referencing columns;
  - drops `text_pattern_ops` for columns with the C collation, and the `pg_trgm` caveat when the extension is installed;
  - states how large the table to index is.
- **Tests** (`crates/explainsql-db/tests/live.rs`, and the connected cases of `crates/explainsql/tests/cli.rs`) run against a database with the fixture schema, named by `EXPLAINSQL_TEST_DATABASE_URL`; the CI's `db` job provides PostgreSQL 16 with HypoPG. A second session counts the rows of the tables touched by `DELETE`, `UPDATE`, `INSERT` and a data-modifying `WITH` before and after each runs with `--allow-dml`: nothing ever changes. Other tests cover:
  - the refusal without `--allow-dml`;
  - a writing function failing in the read-only transaction;
  - several statements and DDL being refused;
  - the timeout and cancellation, after which the connection stays usable;
  - the catalog reads.
- **The proof loop** (`prove.rs`) tests a suggested index before anyone creates it. It uses `t` in the viewer, or `--prove` with `--print`:
  - With HypoPG installed, explainsql creates a hypothetical index inside a read-only transaction and gets the estimated plan with it. `hypopg_reset()` follows unconditionally, since hypothetical indexes outlive transactions. Nothing is built and nothing is locked.
  - Without HypoPG, `--allow-ddl` builds the index for real, without `CONCURRENTLY`, inside a transaction that is rolled back. `SET LOCAL lock_timeout = '2s'` keeps it from waiting behind other sessions, and EXPLAIN ANALYZE measures the statement with it. Building blocks writes to the table, so the viewer first shows the table's size and asks. Only a single `CREATE INDEX` is accepted.
  - `compare.rs` sets the plans side by side: pages read, pages written to temporary files, execution time or estimated cost, and the indexes the second plan uses. Pages decide first: unlike times, they do not depend on what the cache holds. Temporary files come next, then time, and only changes over 10% (and 0.1 ms for times) count; fewer pages but a slower run is mixed. Measured sides run once first only to warm the cache: without that, the run before the index often met a colder cache than the run after it, which followed the build that had just read the whole table. `--runs N` measures each side N times and compares medians. `advisor::verify` records the result: estimated or measured, with the before/after line. A suggestion the planner would not use, or that is not better by more than the noise, drops to low confidence and says so.

  After a run, `r` or an edit with `e` compares the new measured plan with the previous one in the status line.

  A rolled-back `INSERT` or `UPDATE` still leaves dead rows until the next `VACUUM`, as any rolled-back transaction does; `--allow-dml`'s help says that effects outside the table data are not undone.
- **Not yet:** partial indexes from `pg_stats.most_common_freqs`, and `INCLUDE` columns.

## Why not: asking the planner again

A plan shows what the planner chose, not what it turned down. `counterfactual.rs` asks: it plans the statement again with the choice taken away and compares. It is pure: it picks the questions and reads the plans the database returns; `connected.rs` in the binary runs them through `explainsql-db`.

- **Questions.** With `--why-not` and no name, the hot nodes, at most three: sequential scans with a condition that take 10% or more of the runtime, nested loops that ES005 flags or that follow an underestimated outer side, and, with `--measure`, sorts and hashes that spilled. `--why-not TABLE` asks about the sequential scans of a table, or of the table an index belongs to; `y` in the viewer about the selected node.
  - A sequential scan: `enable_seqscan = off`, and `random_page_cost = 1.1` to see whether the planner would take an index by itself.
  - A nested loop: `enable_nestloop = off`.
  - A spill: `work_mem` large enough to stay in memory, a power of two megabytes up to 1 GB, from what the plan shows (three times the sort's disk space, the hash's peak memory times its batches).
- **Settings** (`scenario.rs`). Only planner settings on a fixed list (`enable_*`, the cost constants, `work_mem`, `hash_mem_multiplier`, `effective_cache_size`, the collapse limits, `plan_cache_mode`, `jit`), each with a value of its type, and memory with an explicit unit. `explainsql-db` checks them again and applies them with `set_config(name, value, true)`, names and values bound as parameters, inside the transaction that is rolled back.
- **Matching** (`fingerprint.rs`). Another plan of the statement has other node ids and often another shape. A scan is found again by its relation and alias, a join by the set of relations below it. An index scan counts as using an index only with an index condition: with sequential scans off, the planner may read a whole index without one, in its order, just to avoid the disabled scan.
- **Answers.** Estimated first, which is enough when no alternative exists:
  - *Unusable*: even with sequential scans off, no index serves the condition. The condition and the catalog say why: a cast or a function of the column (with its type), ORs across columns, `<>`, a pattern starting with a wildcard, `LIKE` with a collation other than C and no `text_pattern_ops`, an operator that needs GIN, or indexes that start with another column, are invalid or partial.
  - *Costlier*: the planner can use the alternative and estimates it more expensive, by how much; within 10% is a close call that a small change can flip. Before PostgreSQL 18, the cost of a plan with a disabled node includes 10¹⁰ per node; it is taken out.
  - With `--measure`, the planner's choice and the alternative run the same number of times, after a warm-up run, and compare as above. Not better: *the planner is right*. Better, and the scan's rows were overestimated tenfold or more (or, for a nested loop, its input underestimated): *a misestimate*. Better, with close estimates, and with `random_page_cost = 1.1` the planner picks an index by itself: that plan runs too, and only if it is better as well is the setting suggested (*cost settings*). Otherwise *the planner is wrong* for a reason not found. A spill asks about time: staying in memory but running slower does not help.
  - An alternative that runs past the statement timeout while the planner's choice finished makes the planner right.
- **Approximation.** `enable_*` settings hold for the whole statement, so other scans and joins can change too. The answer lists them and is marked approximate.
- **Advice.** `Analysis::record` keeps the answers and puts them into the advice: an existing index that the planner did not use gets the reason found instead of the likely ones.
- **Tests.** `counterfactual.rs` covers every answer on small plans. `crates/explainsql-db/tests/live.rs` checks that settings hold only inside their transaction and that others are refused; `crates/explainsql/tests/cli.rs` asks about a function of a column, a broad range and a sort that spills, against the fixture database.

## Statements with parameters

A statement with parameters has two kinds of plans. A custom plan is made for the values of one execution. The generic plan is made once for any value. After five custom plans, PostgreSQL switches to the generic plan when its estimated cost is below the custom plans' average cost, each with a charge for planning (`choose_custom_plan` in `plancache.c`), and then keeps it. `params.rs` finds how the plan depends on the values. It is pure: it maps the parameters, picks the values and judges the plans. `params.rs` in the binary runs the plans through `explainsql-db`'s `prepared.rs`.

- **Placeholders.** `$n`, or JDBC's `?` turned into `$n` in order. Literals, quoted identifiers, dollar quotes and comments are skipped, and `??` becomes the `?` operator. PostgreSQL infers each parameter's type from a `PREPARE` (`pg_prepared_statements.parameter_types`).
- **Running.** Each plan comes from its own `PREPARE` and `EXPLAIN EXECUTE`. They run inside a transaction that is rolled back, under `plan_cache_mode` (`force_generic_plan` or `force_custom_plan`, from PostgreSQL 12). The values travel as literals in dollar quotes whose tag they do not contain. The statement is deallocated after the rollback, which does not undo a `PREPARE`, whatever happened. Each run prepares the statement again, so that no cached generic plan carries over from earlier settings. The usual safety holds: the estimated plan first, `READ ONLY` unless `--allow-dml`, and a timeout.
- **Mapping.** A parameter is compared with a column when a scan's index condition, recheck condition or filter does so: `col = $1`, a cast of the column, `$1 <= col` flipped, or `col = ANY (ARRAY[$1, $2])` for an `IN` list. A parameter after `LIMIT`, `OFFSET` or `FETCH FIRST` counts rows. Mapping uses the generic plan with NULL values and `enable_partition_pruning = off`. The generic plan prunes partitions when it starts, using the values it runs with, so with NULL values it would prune them all.
- **Values.**
  - For equality: two most common values, the least common of the most common values, and a histogram value outside them.
  - For a range: the bounds at the 0th, 25th, 50th, 75th and 100th percentiles of the histogram.
  - For a `LIMIT` or an `OFFSET`: fixed row counts.
  - Statistics come from `pg_stats`, of the partitioned table (`pg_partition_root`, inherited) when the scanned table is a partition and the partitioned table has them.

  Each parameter is tried with the others held at a typical value: the one that keeps the most rows, or a page of 10 rows for a `LIMIT`. `--bind` fixes a value. A parameter with no value stops the trials: holding it at NULL would make every custom plan a contradiction.
- **Same plan.** A custom plan is the generic plan when their shapes match. Before comparing, an Append or Merge Append that the generic plan's run-time pruning left with one child is replaced by that child (`fingerprint::pruned_shape`). A custom plan that proves there is no row (a Result whose one-time filter is false, as a range that ends before it starts) is set aside.
- **Judging.**
  - Estimated: values that get another plan make the statement sensitive.
  - Measured (`--measure`): the custom plan and the generic plan run with the same values. The generic plan hurts when it reads at least twice the pages, or, for as many pages, takes twice the time, or runs past the timeout while the custom plan finished. Another plan that does not hurt is harmless.
  - The switch is predicted from the planner's costs. The generic plan is kept whatever the first five values when its cost is below every custom plan's, never when it is above them all, and otherwise depending on those values.
  - Advice to plan each execution (`plan_cache_mode = force_custom_plan`, pgJDBC's `prepareThreshold=0`) comes only when PostgreSQL could switch.
- **Report.** Measured, the report shows the generic plan run with the values it does worst with, so that the findings and the advice (often an index that serves every value) are about that plan. The parameters section comes first, in text, Markdown and JSON.
- **Tests.** `params.rs` covers placeholders, clauses, mapping, values, holding, trials and every verdict on small plans. `fingerprint.rs` covers pruned shapes, and `tests/report.rs` snapshots the report. `crates/explainsql-db/tests/live.rs` checks generic and custom plans, values that try to escape their quotes, writes refused without `--allow-dml`, that nothing stays prepared, and partition pruning. `crates/explainsql/tests/cli.rs` runs a customer's latest orders, whose generic plan walks the index of dates, against the fixture database.

## Locks

`EXPLAIN` does not show locks, but the transaction that `exec.rs` rolls back still holds them after the `EXPLAIN`. `locks.rs` in `explainsql-db` reads them there, between the `EXPLAIN` and the `ROLLBACK`: first the backend's own locks from `pg_lock_status()`, leaving out the one on its own virtual transaction ID (the only lock the transaction holds before the statement), then the catalog for the locked relations, and `pg_locks` with `pg_stat_activity` for other sessions' locks on them. The queries that read them take their own locks only after the first query. `locks.rs` in the core is pure: it reads a capture and the plan.

- **Fast path.** A backend takes weak relation locks (`AccessShareLock`, `RowShareLock`, `RowExclusiveLock`) in slots of its own when no session holds a strong lock on the relation (`lock.c`): 16 before PostgreSQL 18, and from 18 `max_locks_per_transaction` rounded up to a power of two, in groups of 16 that each take a share of the relations (`FastPathLockGroupsPerBackend`). `pg_locks.fastpath` says which got one. Locks that did not fit go to the shared lock table, which statements that take many locks contend for (`LWLock:LockManager`). Locks outside the fast path with free slots left were moved there by a strong lock.
- **By table.** Each lock is grouped under its table: an index under its table, a TOAST table under its main table, a partition and its indexes under the partitioned table at the top (`pg_partition_root`). The plan says which indexes and partitions it reads (`Index Name`, `Relation Name` with its schema). Indexes the plan does not use, not scanned since `pg_stat_database.stats_reset` (`idx_scan` is 0), and that enforce no constraint are named.
- **Conflicts.** The conflict table of the documentation (`LockMode::conflicts_with`) names the commands that would wait for the statement's strongest lock on each table, and, when the plan leaves indexes unused, the commands that lock an index. Other sessions' granted or awaited locks that conflict are notes.
- **Generic plans.** `prepared.rs` prepares the statement and makes its plan in a first transaction, then runs `EXPLAIN EXECUTE` in a second and reads the locks there. A cached generic plan is not planned again: `AcquireExecutorLocks` (`plancache.c`) locks every relation in it, partitions that initial pruning then drops included. Deferring those locks to after pruning was committed for PostgreSQL 18 and reverted. A custom plan is planned for each execution, so its locks are the planner's. `compare_executions` compares the two counts.
- **Waits.** A second connection, opened with the first run it watches, samples `pg_stat_activity` (`wait_event_type`, `wait_event`, and `pg_blocking_pids` for a lock) every 10 ms for the backend and, from 13, its parallel workers (`leader_pid`). The backend's PID is read in each transaction, as a pooler may run it on another backend. `exec.rs` and `prepared.rs` run a measured run again, up to twice, when it waited for another session's lock, and note it.
- **Tests.** `locks.rs` covers grouping, the fast path before and from 18, unused indexes, conflicts, waits and the generic plan's note on small captures, and `tests/render.rs` snapshots the viewer's overlay. `crates/explainsql-db/tests/live.rs` reads the locks of the partitioned fixture table planned with `now()`, of a write, and of its generic and custom plans, and watches a run that waits for another session's row lock and one that a `LOCK TABLE` waits behind. `crates/explainsql/tests/cli.rs` runs `--locks`, with and without `--bind`.

## Writes

The transaction that `exec.rs` rolls back also counts what the statement wrote. `writes.rs` in `explainsql-db` reads `pg_stat_xact_user_tables`, the transaction's own counters of rows inserted, updated, HOT-updated and deleted by table (and, from 16, `n_tup_newpage_upd`), before and after the `EXPLAIN ANALYZE`. The view also holds counts of the backend's earlier transactions that it has not reported to the statistics yet, so the statement's rows are the difference. The read before runs in a savepoint that is rolled back, which releases the locks it takes on the catalog: they would otherwise count among the statement's. The tables written are then read from the catalog: their fillfactor, and for each index its access method, whether it is partial, enforces a constraint or belongs to an index of a partitioned table, its scans, and every column it refers to: `indkey` for its keys and `INCLUDE` columns, and each `:varattno` in the node trees of its expressions and predicate. For a statement that writes, `exec.rs` and `prepared.rs` add `WAL` to `EXPLAIN ANALYZE` from 13. `writes.rs` in the core is pure: it reads a capture, the plan and the statement's text.

- **HOT.** `heap_update` (`heapam.c`) makes an update HOT when the new version fits on the page of the old one and no column of `RelationGetIndexAttrBitmap` changed. From 16, columns that only summarizing indexes (BRIN) refer to do not count, though those indexes still get an entry, which the count of index entries leaves out. `assigned_columns` reads the columns a statement sets from its text: `UPDATE … SET`, `ON CONFLICT … DO UPDATE SET` and `MERGE`'s `UPDATE SET`, inside a `WITH` too, with `(a, b) = …` targets, and without being fooled by commas in `CASE`, function calls or literals. A column the statement sets may keep its value, which does not stop a HOT update, so a blocking index is named only when updates were in fact not HOT. When no index refers to a column it sets, the page had no room: the note gives the fillfactor and how many new versions went to another page.
- **Index entries.** Each row inserted and each update that is not HOT writes an entry in every index of its table; partial indexes make it at most that many.
- **The proof.** With `--prove --allow-ddl`, `writes::prove` drops the blocking indexes that can be dropped alone (not those enforcing a constraint, nor a partition's index attached to its parent's) in a transaction with `lock_timeout = '2s'`, runs the statement there with its writes read, and rolls back.
- **Tests.** `writes.rs` covers the columns a statement sets, blocking indexes with BRIN before and from 16, the page-room note, wording and the proof. `tests/render.rs` snapshots the viewer's overlay. `crates/explainsql-db/tests/live.rs` reads an update blocked by an index twice on one connection, the same update with the index dropped, an update of a column no index refers to, an insert, and checks that a query and an estimated plan write nothing. `crates/explainsql/tests/cli.rs` runs the report and the proof.

## Plan diff

`diff.rs` compares two plans of the same statement: from `explainsql diff`, and for the viewer's status line after a run in connected mode. It is pure, and takes plans from any source and in any format.

- **Matching.** Nodes are matched by the work they do, in three passes, each node at most once:
  1. Within the same scope (the main query, an InitPlan, a SubPlan, a CTE): a scan by what it reads and its alias, a join by the relations below it, any other node by its family (`Gather` and `Gather Merge` are one family, as are the two sorts, the two appends, and `Aggregate` with `Group`) and the relations below it. Relation names below a node have their numbers blanked out, so that an Append over pruned partitions still matches.
  2. Scans by their relation and scope alone: partitions get their aliases in plan order, which pruning and versions change (`events_2025_06` in one plan, `events_6` in the other).
  3. Anything left, wherever it is in the statement.

  When several nodes share a key, they match in plan order.
- **Shapes** (`fingerprint::shape`, `fingerprint::id`). One line per node: its type, join type, strategy, partial mode, parallelism, direction and relationship, the relation, index, CTE or function it reads with numbers blanked out, and the kinds of its conditions. Costs, rows, times, buffers, literal values and aliases are left out: the same plan has the same shape whatever the parameters, the data and the cache, in JSON or text (the corpus checks it on every scenario and version), and when PostgreSQL renames partitions. The id is the shape's 64-bit FNV-1a hash.
- **Changes.** A matched pair of scans changed its access path when its type, index, direction or parallelism differ, or, for bitmap heap scans, the indexes of the bitmaps below; a pair of joins its method, join type or outer side; any other pair its operation (type, strategy, partial mode). Unmatched joins on both sides mean another join order. Unmatched nodes are added or removed, except those their parent's change explains: bitmap index scans, and the Hash of a matched hash join. The same access change on several partitions, and partitions read or no longer read, are told once. Measured plans also compare temporary files (spills), misestimates of 10× or more where they start (not where they carry up the tree), and the work of matched nodes: a change over 10% in pages or time that moves at least 5% of the statement. Time alone, for the same pages, is reported with the pages read from disk: the cache or the load may explain it.
- **Order and verdict.** Structural changes come first, then the others, each by weight: the larger share of the statement's time, pages or estimated cost its nodes take in either plan. The verdict is `compare.rs`'s comparison of the totals followed by the first structural change, or "the plan is the same" when the shapes are.
- **Reports.** Text, Markdown and JSON (`report::diff_*`): the verdict, the shapes, the changes with their evidence, and the plan after with changed nodes marked `~` and new ones `+`. The JSON report is the diff with the label of every node of both plans.
- **Tests.** `diff.rs` covers each kind of change on small plans. `tests/diff.rs` checks that every corpus plan matches itself and its other format node for node, that scans find their relation in another version, that any two plans compare, and what changed from PostgreSQL 12 to 18 in three scenarios; `tests/report.rs` snapshots the text and Markdown reports, and `tests/cli.rs` runs `explainsql diff`.

## Plans over time: server logs

`explainsql logs` reads the plans auto_explain logged and tells, for each statement, which plans it got and when its plan changed. Reading is in `pg/log.rs`, the analysis in `timeline.rs`, both pure; the binary filters and prints.

- **Entries** (`pg::parse_log`). One pass over a jsonlog, a csvlog or a stderr log finds every auto_explain message (`duration: … ms  plan:`) and what the log says about it:
  - jsonlog and csvlog: the record's fields (time, process, user, database, application, query id);
  - stderr: the line prefix, read for a time, a `[pid]`, `user@db` or `user=,db=,app=`;
  - the duration, kept in thousandths of a millisecond so that entries compare exactly;
  - auto_explain's `Query Text:` and, from PostgreSQL 16, its `Query Parameters:` line (a key of JSON plans).

  The plan itself goes through the parsers like any other. An entry whose plan cannot be read is a warning on its line. The robustness tests feed the captured logs cut and mangled.
- **Statements.** Entries are ordered by time, as the log prints it; the time zone is left out, as a log's entries share one. Entries are grouped by the query identifier of the plan, or else of the log record: for an `EXECUTE`, the record's identifier is that of the `EXECUTE`, the plan's that of the prepared query. Without one, entries are grouped by the text: comments, a leading `PREPARE name (types) AS`, literal values and parameters left out, an `IN` list one `?`.
- **Plan changes.** A statement's entries form runs of the same [shape](#plan-diff). Where one run ends and another begins, `diff.rs` compares the last plan of the one with the first of the other. The change also records the runs' median durations, whether another process ran the plan after, and whether the plan after is a generic plan: one that keeps the parameters (`$1`) of a parameterized statement, the plan before not. A statement with four runs or more, and more than twice as many runs as plans, alternates.
- **Order.** Statements whose plan changed come first, by what their costliest change added: the median duration after minus before, times the runs after. The others follow by their total time.
- **sqlcommenter** (`timeline::tags`). The tags of the last comment that holds only `key='value'` pairs are decoded (percent-encoding, `\'`). The statement keeps them, the trace context left out. `--trace` matches the trace id of `traceparent`.
- **Reports** (`report::logs_*`). Text and Markdown: each statement with its plans and changes, and for a switch to a generic plan, the `--params` and `--bind` command that tests it. JSON: the timeline, and every entry with what the log says, its shape and its trace, without the plans.
- **Tests.** `fixtures/logs/` holds a real session, logged by PostgreSQL 16 in the three formats at once (see its README): an index dropped by a migration, a stable report, and a prepared statement that switches to its generic plan. `tests/logs.rs` checks that the three formats give the same entries and timeline. `timeline.rs` covers texts, tags and times on small inputs, `tests/report.rs` snapshots the reports, and `tests/cli.rs` runs the filters.

## Requests and loops

`explainsql requests` groups the statements of server logs into requests and finds the loops in them. Reading is in `pg/log.rs`, the analysis in `requests.rs`, both pure; the binary measures in connected mode and prints.

- **Statements** (`pg::parse_statements`). One pass over a jsonlog, a csvlog or a stderr log reads the messages of statement logging: `duration: … ms  statement: …`, `execute <name>: …`, and `log_statement`'s lines without a duration, whose `duration:` line comes after. The parse and bind steps of the extended query protocol add their durations to the execution they precede, in the same session. Values come from the `parameters:` detail: the record's field, or in stderr the `DETAIL` line of the same process. Each message keeps the session (`%c`, `session_id`) and the virtual transaction (`%v`, `vxid`), unless that says no transaction was open (`3/0`, as a statement that committed on its own logs it). The binary falls back on auto_explain entries, which carry the statement and its values too.
- **Requests** (`requests::profile`). Statements are ordered by time. Those with a `traceparent` tag go together by its trace id, across sessions. The others, per session: a transaction (a virtual transaction id seen more than once) is a request, which takes the `COMMIT` that logs no transaction after it; statements outside one go together while the session was idle no longer than the gap, from the end of one (its log time) to the start of the next (its log time less its duration).
- **Loops.** In each request, statements are grouped by their text without comments, literals and parameters, as `timeline::normalize` writes it. A group of `min_runs` or more that is not transaction control or a setting is a loop; across requests, loops of the same text are one. It is a `LOOP` when the values changed in some request, else a `REPEAT`. The example is the request with the most runs. Runs whose log has no values for their parameters cannot be called a repeat: they are a `LOOP` that is not batched. For statements with their values written in, the literals that changed become `$1`, `$2`, … in the statement that will be prepared, the others stay; runs whose literal lists differ in length (`IN (…)`) cannot be lined up, and a value that changed written as `E'…'` or `B'…'` is not read back.
- **The batched statement** (`requests::batch`). A small tokenizer (words, `$n`, literals, comments, symbols, with the depth of parentheses) reads the statement, and its comments are left out. `= ANY($n::type[])` replaces `= $n` (or `IN ($n)`) when one parameter changed, once, as a term of the statement's own `WHERE`: right after the `WHERE`, an `AND` or an `OR`, followed by the next one, `ORDER BY`, `FOR`, `RETURNING` or the end, with nothing applied to either side (no `NOT`, cast or operator). That holds in a `SELECT` with no `LIMIT`, `OFFSET`, `FETCH`, `GROUP BY`, `HAVING`, `SELECT DISTINCT`, window, set operation or aggregate, and in an `UPDATE` or `DELETE`, after a `WITH` too; then the statement finds the rows of all the runs. Otherwise a `SELECT` goes in `CROSS JOIN LATERAL (…)` over `unnest($n::type[], …)`, each parameter that changed replaced by its column of `batch`, so that each value keeps its own rows; with several, the arrays line up run by run, and a run that repeats another's values is left out. A name right before a value is its type (`date '…'`), so that value cannot become a parameter. An `INSERT` gets advice instead, and an `UPDATE` or `DELETE` that does not fit `= ANY` is left to be written by hand.
- **Proof** (binary, `requests.rs`). It asks the parameters' types (`PREPARE`, `pg_prepared_statements`), runs the batched statement with `Cache::Custom` and the values as array literals (`measure_prepared`), then up to 20 runs one by one (`explain_prepared`, `Mode::Analyze`), all rolled back. `requests::proof` sums the runs' planning and execution times and pages, scaled to every run, against the median of the batched runs, and adds the round trips, timed with `SELECT 1`. The foreign key comes from the column the generic plan compares the parameter with (`params::parameters`) and `pg_constraint` (`Database::references`); `requests::reference` prefers the key whose other table the statement before the loop reads.
- **Reports** (`report::requests_*`). Text and Markdown: each loop with its example request, the batched statement, the proof, the foreign key and the advice. JSON: the requests, the loops and every statement with what the log says, without the values of parameters.
- **Tests.** `fixtures/requests/` holds a real session logged by PostgreSQL 16 in the three formats at once (see its README). `pg/log.rs` checks that the three give the same statements, values, sessions and durations; `requests.rs` covers grouping, loops, the tokenizer and the batched forms on small inputs; `tests/cli.rs` checks that the three formats give the same report and, against the fixture database, the measured proof and the foreign key; `tests/live.rs` reads foreign keys and round trips.

## The costliest statements: pg_stat_statements

`explainsql top` reads `pg_stat_statements` for the current database in a `READ ONLY` transaction that is rolled back, ordered by total execution time, with calls, mean time, shared pages hit and read, and temporary pages written. It first checks that the library is loaded and the extension created, and says which is missing. `top.rs` in the core is pure: it decides which rows can be planned and why not (a utility command, a text pg_stat_statements hides from roles without `pg_read_all_stats`, or one cut at `track_activity_query_size`), and formats the list in text, Markdown and JSON.

In a terminal, the list is part of `explainsql-tui` (`top.rs`). Enter plans the selected statement without running it: with `EXPLAIN (GENERIC_PLAN)` from PostgreSQL 16 when it has `$n` parameters, as pg_stat_statements writes its constants; before 16, or with `p`, the statement goes through the parameter analysis of [Statements with parameters](#statements-with-parameters). The viewer opens on the result and `q` returns to the list. Tests: `top.rs` covers the rows on small inputs, and `crates/explainsql-db/tests/live.rs` reads the view, including as a role that cannot see other roles' texts (`EXPLAINSQL_TEST_READER_URL`).

## Anonymized plans

`anonymize.rs` replaces what a plan tells about the schema and the data, so that it can be shared: the names of tables, indexes, CTEs, aliases, schemas, columns, constraints and triggers, and literal values. Each name gets a replacement by its kind (`table_a`, `index_a`, `column_a`, …), and each literal one of its form (`'value_a'`, a `LIKE` pattern keeping its `%` at either end, other numbers), the same way everywhere in the input. Names that differ only in their numbers keep differing only in their numbers (`table_b_1`, `table_b_2`), because the viewer's folding, `diff` and plan shapes group partitions by their names with the numbers blanked out, and the anonymized plan must group and match as the original does.

It works on the plans themselves, JSON or text, after `normalize()` has removed their wrappers, and rewrites names in node properties, in conditions and in the query text. Function and type names, keywords, `$n` parameters and system names are kept. A property or line it does not know has every name and value in it replaced. Finally the result is parsed again and must have the same nodes as the input, or nothing is printed. `tests/anonymize.rs` runs it on every corpus plan and every captured input form: each must read back with the same nodes, and nothing it named may be left.

## Checks in CI

`explainsql check` is a gate: it exits with 0 when every plan passed, 1 when one failed and 2 when it could not run, and says why in text, Markdown, JSON or SARIF.

- **Core** (`check.rs`, pure). `check()` takes a plan, its analysis and its locked plan, if any, under a `Policy`: `fail_on`, a severity, and `strict`. A plan fails on a finding at least as severe as `fail_on`; on being worse than its locked plan as `compare.rs` judges it, when pages, temporary files or, for two estimated plans, the cost decided; and, under `strict`, on any change of shape. Time alone never fails a plan: for the same pages it changes with the cache and the runner's load, which in CI is noise; it is a note, like a plan that changed and is not worse. Without a locked plan, a plan is new.
- **The lock** (`check::Lock`). Pretty JSON, sorted by name, versioned: for each name, the plan's shape id, pages and estimated cost, and the plan as captured, JSON as JSON and anything else as text, so that `diff.rs` can compare with it and a change reads well in a review. `--update` writes the plans checked and keeps the others. Names are paths from the lock file's directory, with `/`.
- **Binary** (`check.rs`). It collects the files (directories are searched in order: `*.sql` with `-d`, `*.json` and `*.txt` without), reads or runs each one as connected mode does, with the advice checked against the catalog, checks it, and with `--prove` tests the suggested indexes of the plans that failed. A file it cannot read or run is reported on standard error and makes the exit code 2, after the others are checked.
- **Reports** (`report::check_*`). Text: one line per plan with its shape, then why it failed, notes and fixes. Markdown: a table, and for each plan that failed or changed, its diff folded under `<details>`. JSON: every check with its findings and advice. SARIF 2.1.0: the rules and two more, `plan-worse` and `plan-changed`; each finding is a result on its file, an error when it fails the plan and otherwise a warning or a note by severity.
- **Tests.** `check.rs` covers the policy and the lock; `tests/report.rs` snapshots the text and Markdown reports; `tests/cli.rs` runs a plan from new to locked to worse, every format, the exit codes, and the same against the fixture database with `--prove`.
- **The GitHub Action** (`action.yml`, a composite action, with its scripts in `action/`). `install.sh` puts a binary on the runner: the `binary` input as it is, or the release named by `version`, or the one the action was referenced by (`github.action_ref`, such as `v0.3.0`), or the latest. `check.sh` runs `explainsql check` with `--format md` and `--sarif`, keeps both reports in a directory of the run's own, and lets the step pass so that the comment can still be written; the exit code goes to the outputs, and the last step fails with it. `comment.sh` finds the pull request's comment by the hidden marker (with `comment-key` in it) and updates it in place: a failed check posts or updates it, a passing one only updates a comment already there. Pull requests from forks get no comment, since their token cannot write one. The scripts are checked by shellcheck and actionlint in CI, and the `action` workflow runs the action on the plans in `action/test/`: one that matches its lock and passes, and one whose index scan became a sequential scan and fails.

## Releases and documentation

- **Release pipeline** (`.github/workflows/release.yml`). We chose a hand-written workflow over cargo-dist, for three reasons: the smoke tests and dry runs stay fully under our control, development needs no extra tool, and without a Homebrew tap cargo-dist's main extra is not needed. The workflow builds:
  - x86_64 and aarch64 Linux, static with musl (aarch64 through cargo-zigbuild);
  - x86_64 and aarch64 macOS;
  - x86_64 Windows.

  `install/package.sh` puts each binary in an archive with the README, the changelog, the licenses and a SHA-256 checksum. Archive names carry no version, so `releases/latest/download/…` links always work.
- **Smoke tests.** Each archive is installed on a clean runner with `install.sh` or `install.ps1`, from the downloaded artifacts only. Then `install/smoke.sh` or `smoke.ps1` runs:
  - `--version`;
  - `--demo`;
  - a JSON report of a plan file;
  - a psql table on standard input;
  - `--pager` pass-through;
  - the exit code for input that is not a plan.

  Further checks:
  - a tampered checksum is refused;
  - Linux binaries are static, and the aarch64 one runs under QEMU;
  - on fresh Alpine and Ubuntu containers, installing and running `--demo` takes under a minute (download time aside).
- **When it runs.** A `v*` tag that matches the version in `Cargo.toml` publishes a GitHub release: the archives, `SHA256SUMS`, both install scripts and the changelog's section as notes. A manual run, or a branch push that changes the pipeline, is a dry run that publishes nothing.
- **crates.io.** The four crates are published together: `explainsql-core`, `explainsql-db`, `explainsql-tui` and the `explainsql` binary. Each package holds only its sources, a README and the licenses; tests stay out, as they read the corpus outside the crate. The binary's README is the repository's, so its links are absolute. CI packages and builds every crate as crates.io would (`cargo publish --workspace --dry-run`) on every push. On a release tag, the release workflow publishes them in dependency order through Trusted Publishing: crates.io trusts the workflow's OIDC token, and no token is stored. Versions already published are skipped. [RELEASING.md](https://github.com/onplt/explain-sql/blob/main/RELEASING.md) has the steps.
- **Documentation site** (`docs/`, mdBook): getting started, a guide chapter per feature, the command-line reference, troubleshooting, the rule catalog with one page per rule, the design documents and the contributing guide. Findings link to their rule's page (`Rule::doc_url`):
  - in the viewer's details;
  - in Markdown reports;
  - as `rule.docs` in JSON.

  `cargo xtask rule-docs` writes each rule page's example: the scenario that shows the rule best, its plan and explainsql's finding. `cargo xtask check-links` checks every relative link and anchor, and keeps site pages from linking outside `docs/`. The `docs` workflow runs both checks and builds the site on every push. It deploys to GitHub Pages only when run by hand or on a release tag, once Pages is enabled in the repository settings.
- **Demo.** The README's demo is a recording of a real session. `cargo xtask demo --record` builds the release binary and runs it in a tmux pane against the database named by `EXPLAINSQL_TEST_DATABASE_URL` (the fixture schema, without HypoPG, so that `t` builds the index in a rolled-back transaction and measures it). It types the query and a scripted sequence of keys (the verdict, the slowest node, why not, the advice, the measured proof, the locks and the help), waits for each result, and saves every screen as tmux shows it, colors included, to `xtask/demo/recording.json`. `cargo xtask demo` draws the animated SVG (`docs/demo.svg`) from that recording: drawing needs no database and gives the same SVG every time, so CI checks it is current.
