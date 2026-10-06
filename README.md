# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove that it works, without leaving the terminal.**

![ExplainSQL running a slow query against PostgreSQL: the verdict, the slowest node, why the planner uses no index, the suggested index measured before and after in a rolled-back transaction, and the locks the statement takes](https://raw.githubusercontent.com/onplt/explain-sql/main/docs/demo.svg)

`EXPLAIN (ANALYZE, BUFFERS)` is the best tool PostgreSQL gives you for a slow query, and it is hard to read. Times are per-loop averages, buffers are totals, parallel workers and CTEs break simple arithmetic, and the real cause is rarely where the biggest number sits. Web visualizers draw the tree nicely, but they cannot look at your schema, and they cannot tell you whether a fix actually helps.

ExplainSQL is a single binary that closes that loop:

1. **Diagnose.** It works out the time and pages spent in each node, including the parallel, CTE, InitPlan and trigger cases where naive subtraction gets it wrong, and opens on one sentence: where the time went, and why.
2. **Suggest.** [Thirteen rules](https://github.com/onplt/explain-sql/blob/main/docs/rules.md) catch the usual suspects: selective sequential scans, row misestimates, sorts and hashes spilling to disk, expensive nested loops, slow foreign-key checks, plans forced by planner settings, and more. An index advisor writes `CREATE INDEX CONCURRENTLY` statements with their evidence and a confidence level, and says plainly when no index would help.
3. **Prove.** Point it at a database and it runs the query in a transaction that is always rolled back. It tests a suggested index with HypoPG, or, if you allow it, by building the index inside that transaction, and shows before and after. Pages come first, so a warm cache can never pass for an improvement.

Everything else builds on those three steps. It can ask the planner why it turned an index down, show which values of a parameter get a bad generic plan, list the locks a statement takes, explain why an update was not HOT, catch plan regressions in CI, find plan changes and N+1 loops in server logs, and anonymize a plan before you share it.

## Install

On Linux or macOS:

```sh
curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1 | iex
```

The scripts download the binary for your platform from the [latest release](https://github.com/onplt/explain-sql/releases/latest), verify its SHA-256 checksum and put it in `~/.local/bin` (`%LOCALAPPDATA%\explainsql\bin` on Windows). Binaries are built for Linux (x86_64 and aarch64, fully static), macOS (Intel and Apple silicon) and Windows (x86_64).

With Rust 1.85 or later you can also build it yourself, from [crates.io](https://crates.io/crates/explainsql) or from the latest commit:

```sh
cargo install explainsql --locked
cargo install --git https://github.com/onplt/explain-sql explainsql --locked
```

Then try it on the bundled example plan, no database needed:

```sh
explainsql --demo
```

## A quick tour

Open a plan you already have. JSON or text both work, as `EXPLAIN` printed it or still wrapped in psql's table, a server log line or a Markdown fence:

```sh
explainsql plan.json
pbpaste | explainsql
psql -XAtq -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT …" | explainsql
```

Make it psql's pager, and every `EXPLAIN` you run in psql opens in the viewer, while all other output goes to your usual pager:

```sh
export PSQL_PAGER='explainsql --pager'
```

Or let it run the query for you. It shows the estimated plan at once, then runs `EXPLAIN ANALYZE` in a rolled-back transaction:

```sh
explainsql -d "$DATABASE_URL" -f slow.sql
```

In the viewer, `1` jumps to the slowest node, `y` asks the planner why it chose it, `i` shows the suggested index, `t` tests it, `L` shows the locks, `F` draws the plan as an icicle, and `?` lists every key. Outside a terminal, or with `--print`, you get a report instead, as text, Markdown or JSON.

Here is what else it does, each with a chapter in the guide:

| Command | What it tells you |
|---|---|
| `explainsql -d DB -f q.sql --print --prove` | Each suggested index, tested: pages and time before and after. |
| `explainsql -d DB -f q.sql --print --why-not --measure` | Why the planner chose its plan: no usable index (and why not), a misestimate, its cost settings, or simply that it is right. |
| `explainsql -d DB -f q.sql --params --measure` | For a statement with `$1` or JDBC's `?`: which values the generic plan suits, and which it ruins. |
| `explainsql -d DB -f q.sql --print --locks` | The locks it takes, how many miss the fast path, unused indexes it locks anyway, and which migrations would wait for it. |
| `explainsql -d DB -f update.sql --print --allow-dml` | What a write costs: rows per table, whether updates were HOT, the indexes that stopped them, WAL per row. |
| `explainsql top -d DB` | The costliest statements from pg_stat_statements. Pick one and see its plan. |
| `explainsql logs postgresql.log --changed` | From auto_explain logs: when each statement's plan changed, what changed, and what it cost. |
| `explainsql requests postgresql.log -d DB` | N+1 loops in each request of your logs, with the batched statement that replaces them, measured. |
| `explainsql diff before.json after.json` | What changed between two plans of the same statement, node by node. |
| `explainsql check -d DB queries/` | A CI gate: fail when a plan got worse than the one locked for it. Also a [GitHub Action](https://github.com/onplt/explain-sql/blob/main/docs/guide/ci.md#the-github-action). |
| `explainsql anonymize plan.json` | The same plan with table, column and index names and literal values replaced, ready to paste into an issue. |

## Safe by design

When ExplainSQL runs your query, every run happens inside a transaction that is rolled back, with a statement timeout. Anything that writes or locks rows (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, `SELECT … FOR UPDATE`) runs only with `--allow-dml`, and everything else runs `READ ONLY`, so even a function that writes fails. DDL is refused, and building a test index or dropping one for a proof needs `--allow-ddl` plus a confirmation in the viewer. A rollback cannot undo everything, though: sequences keep their new values, and dblink or foreign tables reach other systems. The [connected mode chapter](https://github.com/onplt/explain-sql/blob/main/docs/guide/connected.md) has the details.

PostgreSQL 12 to 18 are supported.

## Documentation

The full documentation lives at **[onplt.github.io/explain-sql](https://onplt.github.io/explain-sql/)**, and its sources are in [`docs/`](https://github.com/onplt/explain-sql/tree/main/docs):

- [Getting started](https://github.com/onplt/explain-sql/blob/main/docs/getting-started.md): install, read your first plan, run your first query.
- [The user guide](https://github.com/onplt/explain-sql/blob/main/docs/guide.md): one chapter per feature.
- [Command-line reference](https://github.com/onplt/explain-sql/blob/main/docs/reference.md): every command, option, environment variable and exit code.
- [Rule catalog](https://github.com/onplt/explain-sql/blob/main/docs/rules.md): what each finding means, when it stays silent, with an example from a real plan.
- [Troubleshooting](https://github.com/onplt/explain-sql/blob/main/docs/troubleshooting.md): common questions and surprises.
- Design: the [vision](https://github.com/onplt/explain-sql/blob/main/docs/VISION.md), the [architecture](https://github.com/onplt/explain-sql/blob/main/docs/ARCHITECTURE.md) and the [roadmap](https://github.com/onplt/explain-sql/blob/main/docs/ROADMAP.md).
- [Changelog](https://github.com/onplt/explain-sql/blob/main/CHANGELOG.md).

## Contributing

Bug reports with a plan that ExplainSQL reads wrongly are the most valuable thing you can send. Run it through `explainsql anonymize` first if it holds anything private. To build and test locally:

```sh
cargo test --workspace            # every test; connected-mode tests need EXPLAINSQL_TEST_DATABASE_URL
cargo run -p explainsql -- --demo # the viewer on the sample plan
```

The [contributing guide](https://github.com/onplt/explain-sql/blob/main/docs/contributing.md) covers the test database, the fixture corpus, adding a rule, the documentation site, the demo recording, fuzzing and releases.

## Status

ExplainSQL is young but complete for its first scope. Version 0.1 shipped the diagnosis, the viewer, the index advisor and the proof loop. Version 0.2 brought it into team workflows: plan diffs, a CI gate and GitHub Action, statements with parameters, server logs, pg_stat_statements, the icicle view and plan anonymization. Lock footprints, the cost of writes and N+1 detection are on `main` for the next release. Feedback and issues are very welcome.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/onplt/explain-sql/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/onplt/explain-sql/blob/main/LICENSE-MIT), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in ExplainSQL by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
