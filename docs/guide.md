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

For the most useful analysis, capture plans with `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`, with `track_io_timing` on: explainsql then says how much of the time went to reading and writing pages, and when the cache was cold. In a terminal, the plan opens in the interactive viewer. Otherwise, or with `--print`, explainsql prints a report: `--format text` (the default), `md` for an issue or a pull request, or `json` for other programs. `--debug-parse` shows what the parser understood.

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
| `F` | The icicle view in place of the tree (below) |
| `r`, `e`, `t`, `Esc` | Connected: run again, edit the query, test a suggestion, cancel |
| `y` | Connected: ask the planner why it chose the selected node |
| `L` | Connected: the locks the statement takes ([below](#what-the-statement-locks)) |
| `W` | Connected: what the statement's writes cost ([below](#what-a-write-costs)) |
| `?`, `q` | Help, quit |

`F` shows the plan as an icicle: the root on top and each node in a box under its parent, as wide as the CPU time spent in it and below it, colored by its own share. Workers of a parallel plan add up under the node that gathers them, InitPlans, SubPlans and CTEs sit under the node they belong to, and time in triggers, outside the tree, is in the title. A plan without timing (`TIMING OFF`, or not run) is drawn by estimated cost, and the title says so. `k` `j` go to the parent and to the widest child, `h` `l` along the row, `Enter` zooms on a box to fill the width with it (`Enter` on that box again zooms out, `g` goes back to the whole plan). Nodes too narrow for a column are folded into their parent and shown as `…`; zooming opens them. The details, search and hotspots work as in the tree, and `F` goes back to it on the same node.

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

## Statements with parameters

An application rarely sends `WHERE customer_id = 4242`. It sends `WHERE customer_id = $1`, or `?` through JDBC, with the value apart. PostgreSQL plans such a statement in one of two ways:

- a **custom plan**, made for the values of one execution;
- the **generic plan**, made once for any value.

A prepared statement gets custom plans for its first five executions. From the sixth on, PostgreSQL switches to the generic plan if it estimates it cheaper than the custom plans were on average, and then keeps it. pgJDBC prepares a statement on the server from its fifth execution (`prepareThreshold`). So a statement that a Java application runs often can end up with the generic plan. That plan may suit some values and ruin others, while the same statement tried in psql with a literal value stays fast.

```sh
explainsql -d shop -c "SELECT * FROM orders WHERE customer_id = ? ORDER BY created_at DESC LIMIT ?" --params --print
explainsql -d shop -f latest_orders.sql --params --measure --print
explainsql -d shop -f latest_orders.sql --bind 1=4242 --bind 2=20 --measure --print
```

explainsql prepares the statement as the application does, then works in four steps:

1. **It maps each parameter** to what the statement does with it. That is the column it is compared with in a scan's conditions (with `=`, a range or an `IN` list), or the `LIMIT` or `OFFSET` it counts rows for.
2. **It picks values to try:**
   - for equality: the most common values, the least common of them, and a value outside them, from `pg_stats` (for a partition, from the partitioned table's statistics);
   - for a range: bounds from across the histogram;
   - for a `LIMIT`: 1, 10, 100, 1,000 and 10,000 rows;
   - for an `OFFSET`: 0, 1,000 and 100,000.
3. **It tries the values one parameter at a time.** The other parameters are held at a typical value: the most common value, the end of a range that keeps every row, a page of 10 rows, or the first page. For each value it compares the custom plan with the generic plan.
4. **With `--measure`, it runs both plans** for each value whose custom plan differs. Each runs `--runs N` times, after one run that warms the cache.

| Verdict | What it means |
|---|---|
| `INSENSITIVE` | Every value gets the generic plan. Whichever plan PostgreSQL uses, it is the same. |
| `SENSITIVE` | Some values get another plan. Measured, for at least one value the generic plan reads at least twice as many pages (or, for as many pages, takes twice as long), or runs past the timeout. Estimated only, the planner prefers another plan for those values, and `--measure` tells how much that matters. |
| `HARMLESS` | Some values get another plan, but measured, the generic plan does less than twice as badly for them. |
| `UNKNOWN` | A parameter has no value to try: it is compared with an expression rather than a column, or its column has no statistics. `--bind N=VALUE` gives it one. |

The report also says whether PostgreSQL would switch to the generic plan after five executions. It switches when the generic plan's estimated cost is below the average cost of the custom plans so far, each with a charge for planning. The outcome therefore depends on the values the first five executions happen to have.

When PostgreSQL would switch and the generic plan does badly, the advice is to plan every execution:

- set `plan_cache_mode = force_custom_plan` for the application's connections (in a JDBC URL, `options=-c%20plan_cache_mode=force_custom_plan`) or for its role;
- or set `prepareThreshold=0` in pgJDBC, for the connection or for one statement.

Either way, each execution is planned again, which costs planning time. An index that serves every value fixes the cause instead, and the report's advice may suggest one.

**The plan in the report.** With `--measure`, it is the generic plan run with the values it does worst with, and the findings and advice are about that plan. Without `--measure`, it is the generic plan, estimated with the typical values.

**Giving values.** `--bind N=VALUE` gives the value of `$N`, and that value is then the only one tried for it. Give a value for every parameter to compare the custom and generic plans for exactly those values, such as one call taken from a log.

**Placeholders.**

- `$1`, `$2`, … are read as PostgreSQL and `pg_stat_statements` write them.
- JDBC's `?` is converted in order when the statement has no `$n`, and `??` becomes the `?` operator.
- A statement with `$n` placeholders cannot run without values, so it needs `--params` or `--bind`.

**Safety and requirements.** Every run is rolled back and every statement explainsql prepares is deallocated, as in connected mode. `--params` needs PostgreSQL 12 or later, for `plan_cache_mode`. It prints a report; the viewer does not show it yet.

**Limits.**

- Values are tried one parameter at a time, so how columns depend on each other is not taken into account.
- A parameter inside an expression (`lower(email) = $1`), an array (`= ANY($1)`) or a `SET` clause gets no value from the statistics. Give one with `--bind`.

## What the statement locks

```sh
explainsql -d shop -f report.sql --locks --print
explainsql -d shop -c 'SELECT * FROM events WHERE created_at > $1' --bind 1=2025-12-20 --locks --print
```

In the viewer, press `L`.

`EXPLAIN` never shows locks. But the transaction explainsql rolls back still holds the statement's locks after the `EXPLAIN`, so explainsql reads them there, from `pg_lock_status()`, just before the rollback releases them. The report says:

- **How many relation locks the statement takes, table by table:** the table, its partitions and its indexes. The planner locks every index of every table it plans, used or not, and every partition it cannot rule out while planning, such as when the partition key is compared with `now()`.
- **How many fall outside the fast path.** A backend takes weak relation locks (`AccessShareLock`, `RowShareLock`, `RowExclusiveLock`) in fast-path slots of its own: 16 before PostgreSQL 18, and from 18 as many as `max_locks_per_transaction` sets, 64 by default. The others go to the shared lock table. Many sessions running such a statement at once contend for it (wait event `LWLock:LockManager`), and slow each other down. The remedy is to take fewer locks: drop the indexes nothing uses, or let the planner rule out partitions.
- **Indexes nothing uses:** locked by every run, not used by this plan, not scanned since the statistics were reset, and enforcing no constraint. Replicas count their own scans, so check theirs before dropping one.
- **What would wait for these locks:** the commands whose locks conflict with the statement's, such as `ALTER TABLE` on its tables or `REINDEX` of any of the indexes it locks, those the plan does not use included. While such a command waits for a long statement, every later run of the statement waits behind it, so the report recommends a `lock_timeout` for schema changes.
- **Other sessions' locks that conflict right now**, such as a migration already waiting behind statements like this one.
- **What the statement waited on as it ran.** A second connection samples `pg_stat_activity` every 10 ms while `EXPLAIN ANALYZE` runs, for the backend and its parallel workers. When the statement waited for another session's lock, the report says how long, and who held it: that time is not the plan's.

With `--no-analyze`, the locks are those that planning takes; `EXPLAIN ANALYZE` adds those of running.

**Statements with parameters.** With `--params` or `--bind`, the report shows the locks of one execution of the generic plan and of one custom plan, with the values the parameters are held at. explainsql prepares the statement and makes its plan in one transaction, then executes it in a second, whose locks it reads: PostgreSQL does not plan a cached generic plan again, and locks every partition in it before run-time pruning drops those the values rule out, while a custom plan locks only the partitions the planner keeps. So as partitions accumulate, every execution of the generic plan takes more locks. Without `--measure`, the plans do not run, and the generic plan's count leaves out the indexes its scans open when it does.

**Measured runs that waited.** With `--locks`, `--measure` or `--prove`, the second connection also watches measured runs. A run that waited for another session's lock runs again, up to twice, and explainsql says so.

**Safety.** It only reads: `pg_lock_status()`, `pg_locks`, `pg_stat_activity` and the catalog, inside the transaction that is rolled back. Reading `pg_locks` takes the lock manager's internal locks for a moment, twice per run. The second connection uses the same settings as the first.

## What a write costs

```sh
explainsql -d shop -c "UPDATE orders SET status = 'shipped' WHERE id = 42" --allow-dml --print
explainsql -d shop -f update.sql --allow-dml --allow-ddl --prove --print
```

In the viewer, press `W` once the measured plan is in.

A statement that writes runs, with `--allow-dml`, inside the transaction that is rolled back. Before the rollback, explainsql reads what it wrote from the transaction's own counters (`pg_stat_xact_user_tables`) and the WAL it wrote from `EXPLAIN (ANALYZE, WAL)`:

```text
Writes
  1 row updated in orders, not HOT: 2 index entries and 209 B of WAL per row.
  orders  1 updated (0 HOT); 2 index entries in 2 indexes
  WAL: 3 records, 209 B.

  LOW     The update of orders was not HOT: the statement sets created_at (orders_created_at_idx),
          which an index refers to, so when the value changes, each such update writes a new entry
          in every index of the table: 2 index entries per row.
```

The report says:

- **The rows each table got**, those of triggers and foreign-key cascades included.
- **Whether updates were HOT.** An update is HOT (a heap-only tuple) when the new version of the row fits on the page of the old one and no index refers to a column whose value changed. Then no index gets an entry. Otherwise every index of the table gets one, for every row, with the WAL that comes with it, and VACUUM has dead index entries to clean up later.
- **What kept them from being HOT.** The columns the statement sets (in `UPDATE … SET`, `INSERT … ON CONFLICT DO UPDATE` and `MERGE`) that an index refers to, in its keys, its `INCLUDE` list, its expressions or its predicate. From PostgreSQL 16, BRIN indexes do not count. An index nothing has scanned since the statistics were reset makes it a medium finding: dropping it would let such updates be HOT. When no index refers to a column the statement sets, the page had no room for the new version: the report says how many went to another page (from PostgreSQL 16) and gives the table's fillfactor.
- **Index entries and WAL per row.** From PostgreSQL 13, explainsql adds `WAL` to `EXPLAIN ANALYZE` for statements that write. The first change to a page after a checkpoint writes the whole page to WAL; when full-page images are most of the records, the report says so, as the statement run again soon after writes far less.

**The proof.** With `--prove --allow-ddl`, explainsql drops the indexes that kept the updates from being HOT inside a transaction, runs the statement again there, reads what it wrote, and rolls back, which brings the indexes back. It leaves out indexes that enforce a constraint and partitions' indexes that belong to an index of the partitioned table. Dropping an index locks its table against reads and writes until the rollback, so explainsql gives up after waiting 2 seconds for the lock.

```text
Without orders_created_at_idx (dropped in a transaction that was rolled back): 1 of 1 update HOT,
no index entries, 81 B of WAL per row.
```

The writes are read for a statement run as it is, not with `--params`. In JSON, they are under `writes`.

## The costliest statements

```sh
explainsql top -d shop
explainsql top -d shop --limit 50 --print
explainsql top -d shop --format json > statements.json
```

`explainsql top` lists the statements that took the most execution time in the database, as [pg_stat_statements](https://www.postgresql.org/docs/current/pgstatstatements.html) counts them: their total time and its share of all the statements' time, calls, the mean time, pages read from the cache or from disk, and pages written to temporary files. In a terminal it is a list to pick a statement from:

- `Enter` shows its plan in the viewer, estimated: nothing runs. pg_stat_statements writes the constants of a statement as `$1`, `$2`, …; from PostgreSQL 16, such a statement gets its generic plan, made for any value (`EXPLAIN (GENERIC_PLAN)`).
- `p` tries values for its parameters, as `--params` does, and shows the report. Before PostgreSQL 16, `Enter` does so too. Only with `--measure` do the plans run, in a transaction that is rolled back, as in connected mode.
- `q` goes back from the viewer to the list, and quits the list.

Elsewhere, or with `--print`, the list is printed: text, Markdown or JSON (`--format`).

The list marks the statements it cannot plan, and says why:

- commands without a plan, such as `VACUUM`, `SET` or `EXPLAIN`, explainsql's own included;
- other users' statements, whose text pg_stat_statements shows only to superusers and members of `pg_read_all_stats`;
- texts cut at `track_activity_query_size` bytes (1024 by default).

**Requirements.** pg_stat_statements must be loaded when the server starts (`shared_preload_libraries = 'pg_stat_statements'`, then a restart) and created in the database (`CREATE EXTENSION pg_stat_statements`); explainsql says which is missing. The list holds the current database's statements. From PostgreSQL 14, only those the application sent: a statement run inside a function counts in the call to the function. Reading the list runs in a `READ ONLY` transaction that is rolled back.

**Limits.** The text pg_stat_statements keeps does not always parse again: a constant written with its type, such as `timestamptz '2026-01-01'`, becomes `timestamptz $1`, which PostgreSQL refuses. Planning such a statement shows the server's error: put the constant back and run the statement with `explainsql -d … -c`.

## Find plan changes in server logs

"It was fast yesterday" is often a plan that changed: after an `ANALYZE`, as the data grew, when a prepared statement switched to its generic plan, or after an upgrade. With [auto_explain](https://www.postgresql.org/docs/current/auto-explain.html), the server logs the plans it ran, and `explainsql logs` reads them: which plans each statement got, when its plan changed, what changed, and what it cost.

```sh
explainsql logs /var/log/postgresql/postgresql-16-main.log
explainsql logs postgresql.json --changed --since 24h
explainsql logs postgresql.csv --query OrderController --format md > incident.md
explainsql logs postgresql.log --trace 4bf92f3577b34da6a3ce929d0e0e4736
```

**Reading the logs.**

- **Formats.** Logs in any of the server's formats: stderr with any `log_line_prefix`, csvlog or jsonlog. Plans can be in text or JSON format, and several files can be read together. `-` reads standard input.
- **What each entry says.** From jsonlog and csvlog records, and from the common stderr prefixes (`%m [%p] %u@%d`, or `user=%u,db=%d,app=%a`), explainsql takes the time, the process, the user, the database and the application. It also reads the duration, the query text and, from PostgreSQL 16, the values a prepared statement ran with.

**Telling statements apart.** A statement is known by its query identifier, which the plans carry when `compute_query_id` is on and auto_explain logs with `log_verbose`. Without one, a statement is known by its text, with its comments, literal values and parameters left out. A prepared statement is known by its query, not by the `PREPARE` around it.

**For each statement**, the report shows:

- its plans, each with how its tables are read, how many runs it had and their median duration;
- each change of plan: when it happened, after how many runs, whether in another session, the median duration before and after, how the plan after compares (pages first, as `explainsql diff` says it), and what changed.

Statements whose plan changed come first, the costliest change first: the time the plan after added over the runs it had. A statement whose plans go back and forth many times is marked `ALTERNATING`: often a plan that depends on the values.

**Generic plans.** A plan that keeps a prepared statement's parameters (`$1`) is its generic plan. The report says when a statement switched to one, with the values it ran with. Those values feed [`--params` and `--bind`](#statements-with-parameters), which tell which values the generic plan suits.

**sqlcommenter tags** in the statements, such as `/*controller='OrderController',action='latest',traceparent='00-…'*/`, say where in the application a statement comes from. Libraries for Spring and Hibernate, Django, Rails and others add them. The report lists the tags, and filters can use them:

- `--query` takes a query identifier, or text found in the statement, the name it was prepared under, or its tags.
- `--trace` takes the trace id of a `traceparent` tag. It keeps the statements that ran in the trace, and says which plan each of the trace's runs got.

**Other filters.** `--changed` keeps only statements whose plan changed. `--since` and `--until` take a time as the log prints it (`2026-10-06 06:00`), or a span back from the log's last entry (`30m`, `24h`, `7d`).

**Setting up auto_explain.** Load it for the whole server with `shared_preload_libraries = 'auto_explain'`, or for one session with `LOAD 'auto_explain'`. Then set:

- `auto_explain.log_min_duration` to the duration from which to log, `0` for every statement;
- `auto_explain.log_analyze`, `log_buffers` and `log_settings` on;
- `auto_explain.log_verbose` on, with `compute_query_id = on`, for the query identifier;
- `auto_explain.log_format` as you like.

With `log_analyze`, every statement is instrumented, logged or not, which slows it down. On a busy server, set `auto_explain.log_timing = off`, or instrument a sample of statements with `auto_explain.sample_rate`.

## Share a plan

A plan tells a lot about a database: the names of its tables, columns and indexes, and the values a statement looked for. `explainsql anonymize` replaces them before a plan goes into a bug report, an issue or a chat:

```sh
explainsql anonymize plan.json > shared.json
pbpaste | explainsql anonymize | pbcopy
explainsql anonymize plan.txt --map names.json   # and keep what each name became
```

- **What changes.** Names of tables, indexes, CTEs, aliases, schemas, columns, constraints and triggers become `table_a`, `index_a`, `cte_a`, `alias_a`, `schema_a`, `column_a`, `constraint_a` and `trigger_a`, then `_b`, `_c` and so on. Names that differ only in their numbers, as partitions do, stay alike: `orders_2025_01` and `orders_2025_02` become `table_b_1` and `table_b_2`, so that the viewer still folds them and `explainsql diff` still matches them. String literals become `'value_a'`, `'value_b'`, … the same way, keeping a `LIKE` pattern's `%` at either end, and numbers in conditions become other numbers of the same form. The same name or value gets the same replacement everywhere, in every plan of the input. A statement's text (`Query Text`) is anonymized the same way, without its comments.
- **What stays.** The node types, estimates, timings, buffers and every other figure, so the plan reads and analyzes as before: the anonymized plan gets the same findings, and compares with another plan as the original does. Function and type names, keywords, `$n` parameters and system names (`pg_catalog`, `public`, `pg_…` relations, `ctid`, the triggers of foreign keys) stay too.
- **What it reads and prints.** Any input explainsql reads, with every plan it holds. The plans come out in the format they were written in, JSON or text, without what surrounded them: psql's table, log lines, a Markdown fence. When an input holds plans of both formats, each comes out in a Markdown fence. A line or a JSON property it does not know has every name and value in it replaced. If the anonymized plans do not read back with the same nodes, nothing is printed.

`--keep-names` replaces only the values. `--map FILE` writes what each name and value became as JSON, to read an answer about the anonymized plan back; keep that file to yourself.

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

`--sarif FILE` writes the SARIF report too, beside a report in another format, so that one run gives both. `--format md` starts with the line `<!-- explainsql check -->`, hidden in a rendered comment, and stays under GitHub's limit for a comment: when the plans do not fit, those that failed come first, then those that changed, and the rest are counted.

### The GitHub Action

The repository is also a GitHub Action. It installs explainsql, runs `explainsql check`, and writes the report on the pull request as a comment, which later runs update in place: a check that fails posts or updates it, and one that passes changes a comment already there to say so, without posting a new one. The report is in the job summary too, and the job fails as the check does.

```yaml
on: pull_request
permissions:
  contents: read
  pull-requests: write        # for the comment
  security-events: write      # only with upload-sarif
jobs:
  plans:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_PASSWORD: postgres
        ports: ["5432:5432"]
        options: --health-cmd pg_isready --health-interval 5s --health-retries 10
    steps:
      - uses: actions/checkout@v5
      - run: psql "$DATABASE_URL" -f schema.sql   # the tables, and data shaped like production's
        env:
          DATABASE_URL: postgresql://postgres:postgres@localhost:5432/postgres
      - uses: onplt/explain-sql@v0.2.0
        with:
          paths: queries/
          database-url: postgresql://postgres:postgres@localhost:5432/postgres
          fail-on: high
          upload-sarif: true
```

Without `database-url`, `paths` are captured plan files and no database is needed.

| Input | Default | |
|---|---|---|
| `paths` | | Plan files, or SQL files with `database-url`; directories are searched. Separated by spaces or new lines. |
| `database-url` | | Run the SQL files against this database. |
| `lock` | `explainsql.lock` | The file of locked plans. |
| `fail-on` | | Also fail a plan with a finding at least this severe. |
| `strict` | `false` | Also fail a plan whose shape changed. |
| `args` | | More arguments for `explainsql check`, such as `--prove` or `--no-analyze`. |
| `comment` | `true` | Comment on the pull request. |
| `comment-key` | `default` | Tells this check's comment from another's, when a workflow checks several sets of plans. |
| `upload-sarif` | `false` | Upload the report to code scanning. |
| `version` | the action's | The explainsql release to install; by default the one the action is referenced by (`@v0.2.0`), or the latest. |
| `binary` | | An explainsql binary to use instead of installing one. |
| `github-token` | `github.token` | The token that writes the comment. |

The outputs are `result` (`passed`, `failed` or `error`), `exit-code`, and the paths of the reports, `report` (Markdown) and `sarif`. The action runs on Linux and macOS runners, and needs explainsql 0.2.0 or later.

A pull request from a fork gets no comment: its token cannot write one, and `pull_request_target`, whose token can, would run the fork's code with the repository's secrets. Its report is in the job summary.

For a single plan, the main command takes `--fail-on` as well: `explainsql --print --fail-on high plan.json` exits with 1 when a finding is at least that severe.

A plan from a database with a handful of rows says little: the planner reads such tables whole. Check against data shaped like production's. PostgreSQL 18 can also restore production's statistics (`pg_restore_relation_stats`, `pg_restore_attribute_stats`), although the planner still sees the size of each table on disk.
