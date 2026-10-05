# Install and use

## Install

Prebuilt binaries are attached to each [GitHub release](https://github.com/onplt/explain-sql/releases) for Linux (x86_64 and aarch64, static), macOS (Intel and Apple silicon) and Windows (x86_64). The install scripts pick the right one, check its SHA-256 checksum, and put `explainsql` in `~/.local/bin` (`%LOCALAPPDATA%\explainsql\bin` on Windows):

```sh
curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh
```

```powershell
irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1 | iex
```

`install.sh --version 0.1.0` installs a given version and `--to DIR` installs elsewhere. Both scripts download from GitHub, so while the repository is private they need a token; download the archive from the release page instead. With Rust 1.85 or later, Cargo builds it from [crates.io](https://crates.io/crates/explainsql), or from the latest commit:

```sh
cargo install explainsql --locked
cargo install --git https://github.com/onplt/explain-sql explainsql --locked
```

Try it on the bundled example plan:

```sh
explainsql --demo
```

## Read a plan

```sh
explainsql plan.json                 # a file: JSON or text
pbpaste | explainsql                 # standard input
psql -XAtq -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT …" | explainsql
```

For the most useful analysis, capture plans with `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`. In a terminal, the plan opens in the interactive viewer. Otherwise, or with `--print`, explainsql prints a report: `--format text` (the default), `md` for an issue or a pull request, or `json` for other programs. `--debug-parse` shows what the parser understood.

## The viewer

The top line is the verdict: the statement's time, where most of it went, and why. Below it are the plan tree (each node's share, time, a bar, rows, the estimate with ▲▼ marks for misestimates, and buffers), the details of the selected node, and a list of findings or advice.

| Keys | What they do |
|---|---|
| `j` `k` `↓` `↑`, `PgDn` `PgUp`, `g` `G` | Move |
| `h` `l` `←` `→`, `Enter` | Fold and unfold. Runs of similar nodes, such as partitions, start folded |
| `/`, `n` `N` | Search node names and conditions |
| `1` … `9` | Go to the slowest nodes |
| `f`, `i`, `Tab` | Show the findings or the advice, and move between the plan and the list |
| `Enter` | In the list: go to the node |
| `c` | Copy the suggested `CREATE INDEX` (OSC 52: works over SSH and in tmux) |
| `x`, `w`, `b` | Time including children, CPU time, buffers |
| `J` `K` | Scroll the details |
| `r`, `e`, `t`, `Esc` | Connected: run again, edit the query, test a suggestion, cancel |
| `y` | Connected: ask the planner why it chose the selected node |
| `?`, `q` | Help, quit |

Colors follow the terminal: true color, 256 or 16 colors, or none with `NO_COLOR`. `--theme light` suits light backgrounds.

## As psql's pager

```sh
export PSQL_PAGER='explainsql --pager'
```

Every `EXPLAIN` you run in psql then opens in the viewer, and any other output goes to `$PAGER` or `less -S`. Add `\pset pager always` to your `.psqlrc` for short plans too.

## Connected mode

```sh
explainsql -d "$DATABASE_URL" -f slow.sql
explainsql -d shop -c "SELECT * FROM orders WHERE customer_id = 42"
```

`-d` takes what psql takes: a URL, `key=value` settings or a database name. The `PG*` environment variables, the service file and `~/.pgpass` apply the same way. The viewer shows the estimated plan at once, runs `EXPLAIN ANALYZE` in the background, then shows the measured plan. The advice is checked against the database's catalog: existing indexes, collations, the columns of foreign keys.

What explainsql promises when it runs your query:

- Every run happens inside a transaction that is **rolled back**, with a `statement_timeout` (`--timeout`, 30 seconds by default).
- Statements that modify data or lock rows (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, `FOR UPDATE`, also inside a `WITH`) run only with `--allow-dml`. Everything else runs in a `READ ONLY` transaction, where even a function that writes fails.
- A rollback does not undo everything. Sequences keep their new values, dblink and foreign data wrappers reach other systems, and rolled-back rows leave dead tuples until the next `VACUUM`.
- One statement at a time, and only kinds `EXPLAIN` accepts. DDL is refused.

`--no-analyze` shows the estimated plan only.

## Test a suggestion

Press `t` on a suggested index, or add `--prove` to a printed report:

```sh
explainsql -d shop -f slow.sql --print --prove
```

- With [HypoPG](https://github.com/HypoPG/hypopg) installed (`CREATE EXTENSION hypopg`), explainsql plans the query with a hypothetical index. Nothing is built or locked, and the result is an estimate.
- Otherwise, `--allow-ddl` lets explainsql build the index for real inside a transaction that is rolled back, and measure the query with it. Building blocks writes to the table while it runs, so the viewer shows the table's size and asks first. It gives up after waiting 2 seconds for its lock.

Either way, explainsql shows before and after: pages read, pages written to temporary files, and execution time, or the estimated cost; and whether the planner used the index. Pages decide first: unlike times, they do not depend on what the cache holds. Times decide only when the pages are the same, and only by more than 10% and 0.1 ms. When measuring, each side runs once more first, only to warm the cache, so that the run without the index does not meet a colder cache than the runs with it. `--runs N` measures each side N times and compares the medians. A suggestion the planner does not use, or that does not make the statement better, drops to low confidence and says so.

## Ask the planner why

The planner picked a sequential scan, but there is an index; it joined with a nested loop; a sort spilled to disk. explainsql can ask the planner again, with the choice taken away, and compare:

```sh
explainsql -d shop -f slow.sql --print --why-not             # the slowest nodes
explainsql -d shop -f slow.sql --print --why-not orders      # the scans of a table, or of an index's table
explainsql -d shop -f slow.sql --print --why-not --measure --runs 3
```

In the viewer, select a node and press `y`.

| The plan chose | explainsql plans again with | It tells |
|---|---|---|
| A sequential scan with a condition | `enable_seqscan = off` | whether an index can serve the condition at all, and if not, why: the condition casts the column or applies a function to it, ORs different columns, uses an operator the index does not serve, a pattern starting with a wildcard, a collation without `text_pattern_ops`, or the index starts with another column, is invalid or partial |
| A nested loop on the hot path | `enable_nestloop = off` | whether a hash or merge join is possible, and better |
| A sort or hash that spilled (with `--measure`) | `work_mem` large enough to stay in memory | whether more memory makes the statement faster |

Without `--measure`, the alternatives are only planned: the answer says how much more expensive the planner estimates them, and whether `random_page_cost = 1.1` (as suits SSDs and cloud volumes) makes it choose the index by itself. With `--measure`, both plans run and are compared, pages first, and the answer says whether the planner is right. When it is not, it says why: a row misestimate (fix the statistics), or its cost settings. A cost setting is suggested only when the plan it leads to has been measured too, and is better.

The settings are planner settings only, from a fixed list, set with `SET LOCAL` semantics inside the transaction that is rolled back: they never outlast the run. `enable_*` settings apply to the whole statement, so other parts of the plan can change as well; the answer says when they did.

## Compare two plans

A plan changed after an index, a statistics update, an upgrade or a rewrite of the query. `explainsql diff` tells what changed, node by node:

```sh
explainsql diff before.json after.json
explainsql diff plans.txt                     # both plans in one input
explainsql diff before.json after.txt --format md
```

The two plans can be in any form explainsql reads, and in different ones. A single input can hold both, one after the other: plans pasted one below the other (a label such as `After:` between them is left out), a JSON array or two JSON documents, two Markdown code fences, two psql results, or two auto_explain entries of a log.

The report opens on a sentence: how the second plan compares, pages first, then time (the estimated cost when the plans were not run), and its main change. Then come the changes, with the most significant first:

| Change | What it means |
|---|---|
| `ACCESS` | A relation is read another way: another scan type, index or direction, or in parallel. The same change on several partitions is told once. |
| `JOIN` | The same relations are joined by another method, or the sides of the join swapped. |
| `ORDER` | The relations are joined in another order. |
| `STRATEGY` | Another variant of the same operation: a hashed aggregate that became sorted, a sort that became incremental. |
| `ADDED`, `REMOVED` | A node only one plan has, such as a Sort an index made unnecessary, or a Gather that runs part of the plan in parallel. Partitions read or no longer read are counted together. |
| `SPILL` | A node started or stopped writing temporary files. |
| `ESTIMATE` | A row estimate became 10× off or more, or stopped being, where the error starts. |
| `WORK` | The same node read more or fewer pages, or took more or less time, by more than 10% and 5% of the statement. A change in time alone, for the same pages, says how many of them came from disk: the cache or the server's load may explain it rather than the plan. |

Last, the plan after, with its changed nodes marked `~` and its new ones `+`, and the nodes only the plan before had.

Nodes are matched by the work they do, not by their position. A scan is found again by the relation it reads, a join by the relations it combines, any other node by its kind and the relations below it. Partitions that PostgreSQL named another way, as versions do, are still matched.

Each plan has a shape: 16 hexadecimal digits that stand for its nodes, what they read and how, without numbers, literal values or aliases. Two plans with the same shape are the same plan, whatever the parameters, the data, the cache, or whether they were printed as JSON or text.

In connected mode, after `r` or `e` runs the statement again, the status line compares the run with the previous one in the same way.

## Check plans in CI

`explainsql check` makes plans a gate in continuous integration: each plan is checked against its findings and against the plan locked for it in `explainsql.lock`. It exits with 0 when every plan passed, 1 when one failed, and 2 when the check could not run.

```sh
explainsql check -d "$DATABASE_URL" queries/ --update      # lock the plans as they are; commit explainsql.lock
explainsql check -d "$DATABASE_URL" queries/               # in CI: fail when a plan got worse
explainsql check -d "$DATABASE_URL" queries/ --fail-on high --prove --format md > comment.md
explainsql check plans/ --format sarif > explainsql.sarif  # captured plans, no database
```

- **What it reads.** With `-d`, SQL files, one statement each (directories are searched for `*.sql`), run as in connected mode: in a transaction that is rolled back, READ ONLY unless `--allow-dml`, with `EXPLAIN ANALYZE`, or with `EXPLAIN` alone under `--no-analyze`. Without `-d`, plan files in any form explainsql reads (directories are searched for `*.json` and `*.txt`).
- **The lock.** `--update` writes each plan to `explainsql.lock` (`--lock FILE` for another file), under its path from the lock file's directory: its shape, its pages, its estimated cost, and the plan itself, a JSON plan as JSON so that a change reads well in a review. Plans not checked in that run stay as they are. Commit the file, and run `--update` again to accept a change.
- **When a plan fails.**
  - It is worse than its locked plan by pages, by temporary files, or, when neither plan was run, by the planner's estimated cost, by more than 10%. Time alone does not fail a plan: on a shared runner it changes from one run to the next for the same pages, so it is a note.
  - With `--fail-on SEVERITY`, a finding at least that severe fails it, locked or not.
  - With `--strict`, a plan whose shape changed fails even when it is not worse: the plan becomes a contract, changed on purpose with `--update`.

  A plan not in the lock yet is new, and fails only on its findings.
- **What it says.** For each plan that failed: why, what changed in the plan (as `explainsql diff` tells it), and the suggested fix; with `--prove` and HypoPG, each suggested index is tested and reported before and after. `--format md` is a pull request comment: the plans in a table, with what changed folded below. `--format sarif` is for code scanning: each finding and each plan worse than its lock is a result on its file, an error when it fails the plan. `--format json` is for other programs.

A GitHub Actions job, against the database the tests use:

```yaml
- name: Check the plans
  run: explainsql check -d "$DATABASE_URL" queries/ --fail-on high --format sarif > explainsql.sarif
- name: Show them in code scanning
  if: always()
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: explainsql.sarif
```

For a single plan, the main command takes `--fail-on` as well: `explainsql --print --fail-on high plan.json` exits with 1 when a finding is at least that severe.

A plan from a database with a handful of rows says little: the planner reads such tables whole. Check against data shaped like production's. PostgreSQL 18 can also restore production's statistics (`pg_restore_relation_stats`, `pg_restore_attribute_stats`), although the planner still sees the size of each table on disk.
