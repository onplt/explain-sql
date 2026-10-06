# Plan changes in server logs

"It was fast yesterday" is very often a plan that changed: after an `ANALYZE`, as the data grew, when a prepared statement switched to its generic plan, or after an upgrade. With [auto_explain](https://www.postgresql.org/docs/current/auto-explain.html), the server logs the plans it actually ran. `explainsql logs` reads them and tells you, for each statement, which plans it got, when its plan changed, what changed, and what it cost.

```sh
explainsql logs /var/log/postgresql/postgresql-16-main.log
explainsql logs postgresql.json --changed --since 24h
explainsql logs postgresql.csv --query OrderController --format md > incident.md
explainsql logs postgresql.log --trace 4bf92f3577b34da6a3ce929d0e0e4736
```

## What you get

Statements whose plan changed come first, the costliest change first, where the cost of a change is the extra time the new plan added over all the runs it had. Here is one from a real session:

```text
CHANGED      SELECT id, status, amount FROM orders WHERE customer_id = ? ORDER BY created_at DESC LIMIT ?
             prepared as latest · query id -1243494815637630641 · 16 runs, 468.2 ms in all
             plan 1  Seq Scan on orders · 10 runs, median 12.2 ms
             plan 2  Index Scan Backward using orders_created_at_idx on orders · the generic plan ·
             6 runs, median 57.9 ms
  06:35:13.659 UTC, line 264
             plan 1 → plan 2 after 5 runs: the generic plan, with $1 = '777', $2 = '10'; median 13.2
             ms → 58.7 ms (4.4× slower)
             Worse: pages 2,417 → 186,053 (77× more), estimated cost 4917 → 1517 (3.2× cheaper). Seq
             Scan on orders became Index Scan Backward using orders_created_at_idx on orders. 1
             other change in the plan.
             REMOVED Sort removed
             → PostgreSQL switched to the generic plan after five executions. With the statement in
             a file, explainsql -d DATABASE -f FILE --params --measure shows which values the
             generic plan suits and what to do; …
```

For each statement:

- **Its plans**, each with how its tables are read, how many runs it had and their median duration.
- **Each change of plan**: when it happened and on which line of the log, after how many runs, whether in another session, the median duration before and after, how the new plan compares (pages first, as [`explainsql diff`](diff.md) would say), and what changed.

A statement whose plans go back and forth many times is marked `ALTERNATING`, which often means a plan that depends on the parameter values.

**Generic plans.** A plan that keeps a prepared statement's parameters (`$1`) is its generic plan. The report says when a statement switched to one, with the values it ran with, and gives you the [`--params` and `--bind`](parameters.md) command that tests exactly those values.

## Reading the logs

- **Formats.** Any of the server's formats: stderr with any `log_line_prefix`, csvlog or jsonlog. Plans can be logged in text or JSON, and several files can be read together. `-` reads standard input.
- **What each entry says.** From jsonlog and csvlog records, and from the common stderr prefixes (`%m [%p] %u@%d`, or `user=%u,db=%d,app=%a`), ExplainSQL takes the time, the process, the user, the database and the application. It also reads the duration, the query text and, from PostgreSQL 16, the parameter values a prepared statement ran with.
- **Telling statements apart.** A statement is known by its query identifier, which plans carry when `compute_query_id` is on and auto_explain logs with `log_verbose`. Without one, it is known by its text, with comments, literal values and parameters left out. A prepared statement is known by its query, not by the `PREPARE` around it.

## sqlcommenter tags

Tags in a statement's comment, such as `/*controller='OrderController',action='latest',traceparent='00-…'*/`, say where in the application the statement comes from. [sqlcommenter](https://google.github.io/sqlcommenter/) integrations for Spring and Hibernate, Django, Rails and others add them, OpenTelemetry's among them. The report lists each statement's tags, and the filters can use them.

## Filters

| Option | Keeps |
|---|---|
| `--changed` | Only statements whose plan changed. |
| `--since`, `--until` | Entries from or up to a time, written as the log prints it (`2026-10-06 06:00`), or a span back from the log's last entry (`30m`, `24h`, `7d`). |
| `--query` | One statement: a query identifier, or text found in the statement, the name it was prepared under, or its tags. |
| `--trace` | The statements that ran in one trace, by the trace id of their `traceparent` tag, with the plan each run in the trace got. |

The report comes as text, Markdown (`--format md`, good for an incident write-up) or JSON.

## Setting up auto_explain

Load it for the whole server with `shared_preload_libraries = 'auto_explain'`, or for one session with `LOAD 'auto_explain'`. Then set:

- `auto_explain.log_min_duration` to the duration from which to log, `0` for every statement;
- `auto_explain.log_analyze`, `log_buffers` and `log_settings` to `on`;
- `auto_explain.log_verbose` to `on`, together with `compute_query_id = on`, for the query identifier;
- `auto_explain.log_format` to whichever you like.

A word of caution: with `log_analyze`, every statement is instrumented, whether it ends up logged or not, and that slows it down. On a busy server, set `auto_explain.log_timing = off`, or instrument only a sample of statements with `auto_explain.sample_rate`.
