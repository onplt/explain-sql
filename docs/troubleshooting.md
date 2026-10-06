# Troubleshooting

Answers to the questions people run into most. If yours is not here, please [open an issue](https://github.com/onplt/explain-sql/issues).

## Reading plans

**"no EXPLAIN plan found in the input"**

ExplainSQL could not find a plan in what you gave it. It reads JSON and text plans, including inside psql output, log lines, Markdown fences and copied result cells, but not the YAML or XML formats. Check that the input really holds the output of `EXPLAIN`, not just the query. `--debug-parse` shows what the parser made of it.

**The plan has no times, and the findings talk about costs.**

The plan was captured without `ANALYZE`, or with `TIMING OFF`. ExplainSQL then works from estimates and pages, and never presents an estimate as a measurement. Capture it again with `EXPLAIN (ANALYZE, BUFFERS)`, or let [connected mode](guide/connected.md) run it for you.

**The times in the viewer do not match what EXPLAIN printed.**

`EXPLAIN` prints each node's time including its children, averaged per loop. The viewer shows each node's own time, for all its loops, with parallel workers, CTEs and InitPlans accounted for. Press `x` to see times including children. The [viewer chapter](guide/viewer.md#times-shares-and-views) explains the difference.

**Times change every time I run the statement.**

They do, because they depend on what is in the cache and how busy the server is. That is why every comparison in ExplainSQL looks at pages first, and why measured runs are preceded by a warm-up run. Use `--runs 5` to compare medians.

**A warning says some lines were not understood.**

ExplainSQL keeps going when a plan has something it does not recognize, such as a property from a newer PostgreSQL version or an extension. The rest of the analysis is still valid. If it looks like something it should understand, please send the plan in an issue.

## The viewer

**It prints a report instead of opening the viewer.**

The viewer opens only when standard output is a terminal and `TERM` is not `dumb`. Redirected output, pipes to another command, and CI logs get a report.

**The colors look wrong.**

Use `--theme light` on a light background. If the terminal shows odd colors, it may claim more colors than it supports: check `COLORTERM` and `TERM`. `NO_COLOR=1` turns colors off.

**`c` does not copy anything.**

`c` uses the OSC 52 escape sequence, which most modern terminals support, sometimes behind a setting. In tmux, enable it with `set -g set-clipboard on`.

## Connected mode

**It cannot connect, but psql can.**

ExplainSQL reads the same settings as psql: `-d`, the service file, the `PG*` variables and `~/.pgpass`. Two things differ in practice. `~/.pgpass` is ignored when other users can read it (`chmod 600 ~/.pgpass`), exactly as libpq does. And with `sslmode=verify-ca` or `verify-full`, the server's certificate must be trusted by `sslrootcert` or the system's trust store.

**"the statement modifies data".**

Statements that write or lock rows run only with `--allow-dml`. They are still rolled back. See [what ExplainSQL promises](guide/connected.md#what-explainsql-promises-when-it-runs-your-statement) before you use it on a database that matters.

**My DDL, or two statements at once, are refused.**

Connected mode runs one statement at a time, of a kind `EXPLAIN` accepts: `SELECT`, `WITH`, `VALUES`, `TABLE`, `INSERT`, `UPDATE`, `DELETE` or `MERGE`.

**A statement with `$1` will not run.**

A statement with placeholders cannot run without values. Add `--params` to try values from your data, or `--bind 1=42` to give one. See [statements with parameters](guide/parameters.md).

**`t` says to install HypoPG or use `--allow-ddl`.**

Testing an index needs either the HypoPG extension in the database (`CREATE EXTENSION hypopg`), or permission to build the index in a rolled-back transaction, which you give by starting ExplainSQL with `--allow-ddl`.

**Building the test index gave up.**

The build waits at most 2 seconds for its lock, so that it never queues behind other sessions and blocks them in turn. Something else was using the table. Try again later, or on a quieter database.

**A run was repeated, with a note about a lock.**

With `--locks`, `--measure` or `--prove`, a second connection watches each measured run. If the run waited for another session's lock, its time says nothing about the plan, so ExplainSQL runs it again, up to twice.

**Does ExplainSQL change my database?**

Not the data: every run is rolled back. But a rollback cannot undo everything. Sequences keep their new values, dblink and foreign tables reach other systems, rolled-back rows leave dead tuples until the next `VACUUM`, and while a statement runs with `--allow-dml` or `--allow-ddl`, it holds its locks. Pointing it at production with only read-only statements is safe; the write and DDL options are meant for development and staging.

## The other commands

**`explainsql top` says pg_stat_statements is missing.**

It needs to be both loaded at server start (`shared_preload_libraries = 'pg_stat_statements'`, then a restart) and created in the database (`CREATE EXTENSION pg_stat_statements`). The message says which part is missing.

**`explainsql top` cannot plan some statements.**

Commands like `VACUUM` and `SET` have no plan. Other users' statements are hidden unless you are a superuser or a member of `pg_read_all_stats`. Long statements are cut at `track_activity_query_size`. And a constant written with its type, such as `timestamptz '2026-01-01'`, becomes `timestamptz $1`, which PostgreSQL refuses to plan; put the constant back and use `explainsql -d … -c`.

**`explainsql logs` finds no plans.**

It reads auto_explain's entries (`duration: … ms  plan:`). Check that auto_explain is loaded and that `auto_explain.log_min_duration` is low enough for your statements to be logged. Statements are told apart best with `compute_query_id = on` and `auto_explain.log_verbose = on`.

**`explainsql requests` puts several requests together, or finds none.**

It needs to know where a request starts and ends. Add `%c %v` to `log_line_prefix` so that sessions and transactions show up in the log, or add sqlcommenter tags with a `traceparent` to your application. Behind a connection pool, statements outside a transaction and without a trace can only be grouped by idle time (`--gap`).

**The GitHub Action did not comment on a pull request from a fork.**

That is deliberate: a fork's token cannot write comments, and the alternative, `pull_request_target`, would run the fork's code with your secrets. The report is in the job summary.

## Reporting a bug

The most useful bug report includes the plan. Please:

1. run `explainsql --debug-parse` on it, to check whether the parser read it as you expected;
2. anonymize it with `explainsql anonymize plan.txt > shared.txt` if it holds anything private;
3. attach it to an [issue](https://github.com/onplt/explain-sql/issues) with `explainsql --version` and your PostgreSQL version.
