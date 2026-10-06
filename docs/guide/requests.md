# N+1 loops in requests

An ORM that loads related rows one parent at a time runs the same statement again and again in a single request, with a different value each time: a customer's orders, then the items of each order, one order at a time. Each run is fast, so its plan looks fine, and so does any list of statements sorted by mean time. The cost only shows when you look per request, and that is exactly what `explainsql requests` does. It groups the statements in your server logs into requests, finds these loops, and writes the batched statement that does the work of all the runs at once.

```sh
explainsql requests /var/log/postgresql/postgresql-16-main.log
explainsql requests postgresql.json -d shop               # and measure each batched statement
explainsql requests postgresql.csv --min-runs 10 --format md > n-plus-one.md
```

## What you get

Loops come first, those with the most runs first. Here is a real one, from a Spring application:

```text
LOOP    SELECT id, product_id, quantity FROM order_items WHERE order_id = ?
        5 runs in each of 2 requests, 10 in all, 224.1 ms in the database; $1 changed from run to
        run (order_id).
        After SELECT id, status, created_at FROM orders WHERE customer_id = ? ORDER BY created_at
        DESC LIMIT ?
        In the request of OrderController#latest, trace 4bf92f3577b34da6a3ce929d0e0e4736, at
        2026-10-06 16:04:12.325 UTC: 9 statements, 146.8 ms in the database:
           1× SELECT id, status, created_at FROM orders WHERE customer_id = ? ORDER…    32.4 ms
           5× SELECT id, product_id, quantity FROM order_items WHERE order_id = ?      113.6 ms ◀
           3× SELECT id, name, price FROM products WHERE id = ?                        0.730 ms
        Batched:
          SELECT id, product_id, quantity FROM order_items WHERE order_id = ANY($1)
        Then select order_id too, to tell which rows go with which value.
        → JPA: JOIN FETCH or an @EntityGraph on the association, or @BatchSize on it
        (hibernate.default_batch_fetch_size for all).
```

A statement that ran `--min-runs` times or more (3 by default) in one request is a loop. It is a `LOOP` when its values changed from run to run, and a `REPEAT` when they were the same every time; a repeat is best fixed by reading the value once per request and keeping it. For each loop, the report shows:

- how many runs it had in how many requests, and their time in the database: parse, bind and execute together;
- the parameter that changed, and the column it is compared with;
- the statement just before the loop, which is often the one that read the parents;
- the request it looped most in, statement by statement;
- **the batched statement**:
  - `col = ANY($1)` in place of `col = $1` or `col IN ($1)`, when that comparison is a plain term of the statement's own `WHERE`. This is what an ORM's batch fetching sends.
  - When each value must keep its own rows, because of a `LIMIT`, an aggregate, a `GROUP BY`, a `DISTINCT` or a window function, or when the value is cast or computed, the statement goes into a `LATERAL` subquery over `unnest($1)`, so that each value keeps its own `LIMIT` or count.
  - An `INSERT` per row is not rewritten; the report tells you how to send the rows together instead. An `UPDATE` or `DELETE` is rewritten only with `= ANY`.
  - A loop whose log has no values for its parameters is shown, but not batched.
- **what to change in the application**: JPA (`JOIN FETCH`, `@EntityGraph`, `@BatchSize`), Django (`select_related`, `prefetch_related`) or Rails (`includes`). When the statements carry sqlcommenter's `framework` tag, only that framework's fix is shown.

## Measuring the batched statement

With `-d`, ExplainSQL runs each loop's batched statement with all the values from the request it looped most in, and the runs one by one (at most 20 of them, scaled up to all). Every run is prepared the way the application ran it, and rolled back, `READ ONLY` unless `--allow-dml`. The batched statement runs first, after a warm-up run, so both sides find the data in the cache.

The report then compares their time and pages, says when the batched statement reads its tables differently (a large array can turn index scans into a sequential scan or a hash join), and adds the network round trips: the median time of a `SELECT 1` from your machine, once for the batched statement and once per run. It also names the foreign key behind the loop, from the column in the generic plan: the rows that reference one parent (a collection, `@OneToMany`), or the parent of each row (`@ManyToOne`).

Measuring needs PostgreSQL 12 or later. `--runs` takes the median of several runs of the batched statement, and `--limit` sets how many loops are shown and measured (10 by default).

## Logging the statements

The server must log every statement of the requests, with its duration:

- On a staging server, set `log_min_duration_statement = 0`. Statements that the driver prepares (the extended query protocol, which JDBC, psycopg 3 and most drivers use) are logged with their values in a `DETAIL: parameters:` line.
- On production, `log_transaction_sample_rate` (PostgreSQL 12 and later) logs a sample of whole transactions, which is exactly the unit you need.
- `log_statement = all` with `log_duration = on` works too, and so do logs with auto_explain entries at `auto_explain.log_min_duration = 0`.

Logs can be stderr with any `log_line_prefix`, csvlog or jsonlog, and several files can be read together.

## How statements are grouped into requests

Statements go together:

1. by the trace id of their sqlcommenter `traceparent` tag, such as `/*controller='OrderController',action='latest',traceparent='00-4bf9…-00f0…-01'*/`, which sqlcommenter and OpenTelemetry integrations for Spring and Hibernate, Django, Rails and others add;
2. otherwise, by the transaction they ran in, within their session: `%v` in `log_line_prefix`, or the field in jsonlog and csvlog;
3. otherwise, by their session (`%c`, or the process `%p`), split wherever it sat idle for longer than `--gap` (50 ms by default).

For the second and third, add `%c %v` to `log_line_prefix`, for instance `'%m [%p] %q%u@%d %c %v '`. Behind a connection pool, statements outside a transaction and without a trace can only be grouped by idle time, which may put two requests together.

## Privacy

The reports leave out the values the statements ran with, except those written into a statement's text. Keep in mind that logging every statement with its values writes your application's data to the log: keep such logs wherever that data is allowed to be.
