# Getting started

This page takes you from nothing to your first proven fix. It should take a few minutes.

## Install

The quickest way is the install script. It downloads the right binary for your platform from the [latest GitHub release](https://github.com/onplt/explain-sql/releases/latest), checks its SHA-256 checksum, and puts `explainsql` in `~/.local/bin`:

```sh
curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell, it goes to `%LOCALAPPDATA%\explainsql\bin`:

```powershell
irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1 | iex
```

Both scripts take options. To pin a version or choose where the binary goes:

```sh
curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh -s -- --version 0.2.0 --to ~/bin
```

```powershell
& ([scriptblock]::Create((irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1))) -Version 0.2.0 -To C:\tools
```

Prebuilt binaries cover Linux (x86_64 and aarch64, fully static, so they run on any distribution, Alpine included), macOS (Intel and Apple silicon) and Windows (x86_64). You can also download an archive from the [releases page](https://github.com/onplt/explain-sql/releases) and unpack it yourself; each archive comes with a `.sha256` file, and the release has a `SHA256SUMS` list.

If you have Rust 1.85 or later, Cargo works too:

```sh
cargo install explainsql --locked                                           # the latest release, from crates.io
cargo install --git https://github.com/onplt/explain-sql explainsql --locked # the latest commit
```

Check that it runs, then open the bundled example plan:

```sh
explainsql --version
explainsql --demo
```

The demo is a real plan with a real problem: a nested loop that scans a whole table once per outer row. Move around with the arrow keys or `j` and `k`, press `?` for the keys and `q` to quit.

## Read your first plan

Give it any plan you have:

```sh
explainsql plan.json                 # a file, JSON or text
pbpaste | explainsql                 # the clipboard (xclip -o or wl-paste on Linux)
psql -XAtq -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT …" | explainsql
```

You do not have to clean the plan up first. ExplainSQL finds it inside psql's aligned, wrapped, expanded or CSV output, inside server log lines (stderr, csvlog or jsonlog), inside a Markdown code fence, or inside a result cell copied from pgAdmin or another GUI client, with or without the `QUERY PLAN` header and the `(N rows)` footer.

### Capture plans that say more

The more the plan contains, the more ExplainSQL can tell you. The best capture is:

```sql
SET track_io_timing = on;   -- a superuser setting; or turn it on in postgresql.conf
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, FORMAT JSON) SELECT …;
```

- `ANALYZE` runs the statement and gives real times and row counts. Without it you get estimates only, and findings are based on costs.
- `BUFFERS` gives the pages each node read. ExplainSQL leans on pages because, unlike times, they do not depend on what happens to be cached.
- `VERBOSE` gives output columns and qualified names, which makes the advice more precise.
- `SETTINGS` lists planner settings changed from their defaults, which is how [ES013](rules/ES013.md) catches a forgotten `enable_seqscan = off`.
- `track_io_timing` lets ExplainSQL say how much of the time went to reading from disk, and whether the cache was cold.

JSON and text are equally welcome. Text is what psql and auto_explain print by default, and it is what people usually paste into chats and issues.

Careful: `EXPLAIN ANALYZE` really runs the statement. For an `UPDATE` or `DELETE`, wrap it in `BEGIN; … ROLLBACK;`, or let ExplainSQL run it for you in [connected mode](guide/connected.md), which always rolls back.

## Make sense of the screen

![The viewer](demo.svg)

From top to bottom:

- **The verdict.** One sentence: how long the statement took, where most of the time went, and the finding about that node. Often that is all you need to read.
- **The statement line.** Planning and execution time, pages read and the share that came from the cache, and when they matter, time in triggers, in JIT compilation, outside the plan tree, and spent reading from disk.
- **The plan.** One row per node: its share of the runtime, its own time, a bar, the node, actual rows (with `×loops` when it ran more than once), the estimate, and the pages it read itself. A `▲` or `▼` marks rows 10× or more above or below the estimate, and a `!` in the margin marks a node with a finding. Similar siblings, such as the scans of 300 partitions, start folded into one row.
- **The details** of the selected node: every figure, its conditions, its findings, and what the planner said when you asked it why.
- **Findings and advice.** `f` shows the findings, `i` shows the suggested indexes and rewrites. `Tab` moves into the list, and `Enter` jumps to the node.

All the times are the node's own, which is the time spent in the node minus the time spent in its children, worked out correctly for parallel plans, CTEs and InitPlans. Press `x` to see times including children, `w` for CPU time summed across parallel workers, and `b` to rank nodes by pages instead of time. [The viewer](guide/viewer.md) chapter covers every key.

### Or get a report

When the output is not a terminal, or with `--print`, you get a static report instead of the viewer:

```sh
explainsql --print plan.json                    # text, for the terminal
explainsql --print --format md plan.json        # Markdown, for an issue or a pull request
explainsql --print --format json plan.json      # JSON, for scripts and other tools
```

See [reports and output](guide/reports.md) for what each format holds.

## Run your first query

Connected mode is where ExplainSQL can do the most. Point it at a database and a statement:

```sh
explainsql -d "$DATABASE_URL" -f slow.sql
explainsql -d shop -c "SELECT * FROM orders WHERE customer_id = 42"
```

`-d` takes whatever psql takes: a URL such as `postgresql://app@db.internal/shop`, `key=value` settings, or just a database name. The `PG*` environment variables, `~/.pg_service.conf` and `~/.pgpass` work exactly as they do for psql. If psql connects, ExplainSQL connects.

The viewer shows the estimated plan right away, runs `EXPLAIN ANALYZE` in the background (press `Esc` to cancel), then swaps in the measured plan. The statement runs inside a transaction that is always rolled back, and it runs read-only unless you pass `--allow-dml`. [Connected mode](guide/connected.md) explains every safety rule.

Now try the loop from the demo at the top of the page:

1. Press `1` to go to the slowest node.
2. Press `y` to ask the planner why it chose that node. For a sequential scan, it plans the statement again with sequential scans turned off, and tells you whether any index could serve the condition at all.
3. Press `i` to see the suggested index.
4. Press `t` to test it. With [HypoPG](https://github.com/HypoPG/hypopg) installed, the test is instant and nothing is built. Without it, start ExplainSQL with `--allow-ddl` and it will offer to build the index inside the rolled-back transaction and measure the query with it.
5. Read the result in the details: *"Before → after: Pages 2,420 → 15 (161× fewer), execution 9.98 ms → 0.061 ms (164× faster)"*. Press `c` to copy the `CREATE INDEX CONCURRENTLY` statement.

The same, as a report you can paste into a ticket:

```sh
explainsql -d "$DATABASE_URL" -f slow.sql --allow-ddl --print --prove --format md
```

## Where next

- [Use it as psql's pager](guide/pager.md), so that it is there whenever you type `EXPLAIN`.
- Learn what the [rules](rules.md) look for.
- Browse the [user guide](guide.md) for the rest: parameters, locks, writes, CI, logs and more.
