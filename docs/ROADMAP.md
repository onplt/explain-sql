# Roadmap

Status: Phases 0 and 1 are complete, Phases 2 and 3 are code-complete, and Phase 4 is in progress (see [Development phases](#development-phases)). Time estimates assume a single developer working part-time.

## v0.1 scope

### In scope

1. **PostgreSQL 12–18** (best effort for 9.6–11), JSON and text formats, with input normalization.
2. **Correct metrics:** inclusive and exclusive time and buffers, including the parallel, CTE and trigger cases, plus estimate-vs-actual misestimate factors.
3. **TUI:** a one-line verdict, a plan tree with heat bars, a detail pane, jumps to the top three hotspots, search, collapse and expand, and folding of similar siblings (a thousand partitions become `Seq Scan on orders_p* ×997`).
4. **About a dozen high-precision rules**, each with an ID and a documentation page. See the [rule catalog](rules.md).
5. **Index advisor** for three patterns (selective sequential scan, nested-loop inner join key, sort + limit top-N) plus slow foreign-key triggers.
6. **Connected mode:** safe execution (transaction + rollback, `statement_timeout`, `--allow-dml` for data-modifying statements), libpq-compatible connection settings, and catalog and statistics reads.
7. **Proof loop:** HypoPG when available, otherwise opt-in rollback verification (`--allow-ddl`), with a before/after summary.
8. **`--demo`:** a bundled example plan, so anyone can try the tool without a database.
9. **Non-interactive output:** a static report when stdout is not a terminal, and `--format json|md`.
10. **Distribution:** prebuilt binaries on GitHub Releases (macOS arm64/x64, Linux musl x64/arm64, Windows), a Homebrew tap, `cargo install` and an install script.

### Explicitly out of scope for v0.1

- MySQL, MariaDB, SQLite and other engines (v0.3 and later).
- A SQL editor, schema browser or query history. This is not a database client.
- AI or LLM features in the core.
- Plan hosting or sharing.
- A full tree-diff algorithm and a flame/icicle view (v0.2).
- `auto_explain` log mining, a pg_stat_statements browser and a GitHub Action (v0.2–v0.3).
- A plugin system, a theme engine, YAML/XML plan formats, and web or editor front ends.

## User experience

**Plan first, connection optional, more capable step by step.** Three entry points lead to the same screen:

- **Offline, zero configuration:**

  ```sh
  explainsql plan.json
  pbpaste | explainsql
  psql -XAtq -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT …" | explainsql
  ```

  Inside psql, `\g | explainsql` works too. No credentials and no network are needed, and advice is labeled "unverified".
- **psql pager mode:** `PSQL_PAGER='explainsql --pager'` together with `\pset pager always`. EXPLAIN output becomes interactive, and anything else goes to your regular pager.
- **Connected:** `explainsql -d "$DATABASE_URL" -f slow.sql` shows the estimated plan immediately, runs `ANALYZE` in the background (with elapsed time, and `Esc` to cancel), then swaps in the actual numbers. `e` opens the query in `$EDITOR`; saving re-runs it and shows the difference from the previous run.

Screen principles:

- **The first screen answers the question:** a verdict line and the top three hotspots. The tree comes second.
- Layout: a summary strip at the top (planning and execution time, cache hit ratio, JIT, triggers, unattributed time); the tree on the left (exclusive-time bar, estimate-vs-actual factor, buffers); details on the right; findings at the bottom, each linked to its node.
- Keys: vim-style navigation (`j/k`, `h/l`, `g/G`, `/`, `n/N`), `1`–`3` for hotspots, `x` inclusive/exclusive, `w` wall-clock/CPU, `b` buffers view, `i` advisor, `t` test a suggestion, `c` copy (OSC 52, so it works over SSH and in tmux), `e` edit, `r` re-run, `?` help.
- Deep plans: a "hot path only" filter, focus on a subtree, and a layout that stays usable at 80×24.
- Accessibility: truecolor → 256 colors → 16 colors → `NO_COLOR`; symbols alongside color (▲ ⚠); automatic light/dark theme.

## Development phases

Phase 0 is complete: the workspace skeleton, CI, and 65 scenarios captured on PostgreSQL 12–18 (see [fixtures/README.md](../fixtures/README.md)). Phase 1 is complete: the plan IR and the JSON and text parsers meet all four exit criteria (see [Testing the parsers](ARCHITECTURE.md#testing-the-parsers)).

Phase 2 is code-complete. The metrics engine, the twelve rules and the static report are in place. Exclusive times agree with pev2 and explain.depesz.com within 5% on every node of 24 reference plans, except one deliberate difference in rounding (see [tools/cross-check](../tools/cross-check/README.md)). The report snapshots are stable. The remaining exit criterion is a quiet alpha with a few DBAs.

Phase 3 is code-complete. The viewer has the layout, the virtualized tree, details, findings, search, folding (including similar siblings), view modes, colors, pager mode and `--demo`. A frame of a 5,000-node plan takes about 0.3 ms. `TestBackend` snapshots cover 120×40 and 80×24. The keys for the connected mode (`t`, `e`, `r`) come with Phase 4.

Phase 4 is in progress, in three steps:
- 4a, the offline advisor, is done. Conditions are read by the hand-written reader in `expr.rs`. Candidates come from the scan rules and from three more patterns (top-N sorts, partitions, correlated subqueries), followed by the rewrites and explanations of why no index would help. In the viewer, `i` shows the advice and `c` copies a statement. All 7 `advice: none` scenarios get no suggestion on every version and in both formats.
- 4b, the connected mode, is next.
- 4c, the proof loop, follows.

| Phase | Estimate | Scope | Exit criteria |
|---|---|---|---|
| 0: Fixture corpus | 1 week | Docker matrix for PostgreSQL 12–18; 40+ scenarios (parallel, CTE, InitPlan, partitions, FK triggers, disk spills, JIT, nested loops, Memoize), each captured as JSON and text; CI skeleton (fmt, `clippy -D warnings`, tests) | `cargo xtask gen-fixtures` regenerates the whole corpus with one command |
| 1: Parsers | 2–3 weeks | normalize / JSON / text → raw tree → IR | 100% of the corpus parses; JSON and text produce the same plan shape, estimates and row counts; one hour of fuzzing without a crash; no unknown fields lost |
| 2: Metrics engine | 2 weeks | Inclusive/exclusive time and buffers, parallel/CTE/trigger handling, misestimates, hotspots; rules ES001–ES012; `--print` with text, Markdown and JSON output | Within ±5% of pev2 and explain.depesz.com on 20 reference plans; stable snapshots; a quiet alpha with a few DBAs |
| 3: TUI | 3 weeks | Layout, virtualized tree, details, findings, search, folding, theming, pager mode, `--demo` | Under 16 ms per frame on a 5,000-node plan; `TestBackend` snapshots; usable at 80×24 |
| 4: Advisor, connected mode, proof loop | 3 weeks | Predicate parser, three patterns, negative rules, safe executor, catalog reads, HypoPG/rollback prover, before/after diff | Zero suggestions on scenarios marked `advice: none`; an integration test proving that data-modifying statements are never committed |
| 5: Release hardening | 1–2 weeks | Release pipeline, Homebrew tap, README with a demo recording, rule documentation site, Windows smoke tests; optionally a browser playground | From a clean machine, install to a running `explainsql --demo` in under a minute |

## After v0.1

- **v0.2: fit into team workflows.** Real plan diffs (tree matching); `explainsql check` with exit codes and SARIF/Markdown output; a GitHub Action that comments plan regressions on pull requests; a pg_stat_statements entry screen (using `GENERIC_PLAN` on PostgreSQL 16+ for queries without parameter values); a flame/icicle view; an `anonymize` command.
- **v0.3: become a tool for AI agents.** `explainsql mcp`, which exposes the deterministic analysis engine to coding agents; `auto_explain` log mining with query fingerprinting; beta MySQL 8.x support (the `EXPLAIN ANALYZE` tree format and the 8.3+ JSON format).
- **v0.4 and later:** MariaDB; an editor extension built on the WebAssembly core; ORM bridges (for example, turning Hibernate's `?` placeholders into `$n` and planning them with `GENERIC_PLAN`).
