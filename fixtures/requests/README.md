# Requests

Real server logs with statement logging, which `explainsql requests` and its
tests read: the same session written by PostgreSQL 16 in the three formats it
logs to.

- `postgresql.log`: stderr, with `log_line_prefix = '%m [%p] %q%u@%d %c %v '`:
  Debian's default, and the session and virtual transaction ids.
- `postgresql.csv`: csvlog.
- `postgresql.json`: jsonlog.

[`session.sql`](session.sql) is the session: an application's four requests
on one connection, as an ORM runs them.

- Two requests of `OrderController#latest`, tagged by sqlcommenter with their
  own `traceparent`. Each reads a customer's latest five orders, then the
  items of each order one order at a time (N+1: `order_items.order_id` has no
  index, so each run scans the table), then the same product three times.
  The statements go with the extended query protocol (psql's `\bind`), so
  the log has their parse, bind and execute steps and their values in
  `DETAIL: parameters:` lines.
- A transaction without tags whose values are written into the text: three
  customers, then a count of each one's orders.
- Statements without tags or a transaction, after an idle pause: the same
  setting read four times.

The logs were captured from a server with the [fixture schema](../schema.sql)
that logs every statement to all three formats at once:

```sh
pg_createcluster 16 requests -p 5433 -o logging_collector=on \
  -o "log_destination=stderr,csvlog,jsonlog" -o log_filename=postgresql.log \
  -o "log_line_prefix=%m [%p] %q%u@%d %c %v " \
  -o log_min_duration_statement=0 -o max_parallel_workers_per_gather=0
pg_ctlcluster 16 requests start
psql -p 5433 -c "CREATE DATABASE shop"
PGOPTIONS='-c log_min_duration_statement=-1' psql -p 5433 -d shop -f ../schema.sql
PGAPPNAME=shop-api psql -p 5433 -d shop -f session.sql
```

The files are as the server wrote them, startup and shutdown lines included.
Times, process ids and durations are those of that run.
