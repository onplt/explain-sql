# Server logs

Real server logs with auto_explain entries, which `explainsql logs` and its
tests read: the same session written by PostgreSQL 16 in the three formats
it logs to.

- `postgresql.log`: stderr, with Debian's default `log_line_prefix` (`%m [%p] %q%u@%d `).
- `postgresql.csv`: csvlog.
- `postgresql.json`: jsonlog.

[`session.sql`](session.sql) is the session: an application's statements, run
twice, with text plans and then JSON plans.

- A customer's latest orders, tagged by sqlcommenter: served by an index
  until a migration drops it.
- A daily report whose plan never changes.
- A prepared statement that switches to its generic plan after five
  executions, from a sequential scan and a sort to a backward scan of the
  index on `created_at` that filters most rows: five times slower.

The logs were captured from a server with the [fixture schema](../schema.sql)
that logs to all three formats at once:

```sh
pg_createcluster 16 logs -p 5433 -o logging_collector=on \
  -o "log_destination=stderr,csvlog,jsonlog" -o compute_query_id=on \
  -o track_io_timing=on -o log_filename=postgresql.log \
  -o max_parallel_workers_per_gather=0
pg_ctlcluster 16 logs start
psql -p 5433 -c "CREATE DATABASE shop"
psql -p 5433 -d shop -f ../schema.sql
psql -p 5433 -d shop -v format=text -f session.sql
psql -p 5433 -d shop -v format=json -f session.sql
```

The files are as the server wrote them, startup lines included. Times,
process ids and durations are those of that run.
