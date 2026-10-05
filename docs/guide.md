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

Either way, explainsql shows before and after: execution time or estimated cost, pages read, and whether the planner used the index. A suggestion that does not help drops to low confidence and says so.
