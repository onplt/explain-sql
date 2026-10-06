# Statements with parameters

Here is a classic: a query is slow in production, you copy it into psql with a real value, and it runs in a millisecond. Nothing is wrong with the query. What differs is how your application sends it.

An application rarely sends `WHERE customer_id = 4242`. It sends `WHERE customer_id = $1`, or `?` through JDBC, with the value on the side. PostgreSQL can plan such a statement in two ways:

- a **custom plan**, made for the values of one execution, which is what you get in psql;
- the **generic plan**, made once to work for any value.

A prepared statement gets custom plans for its first five executions. From the sixth on, PostgreSQL switches to the generic plan if it estimates it cheaper than the custom plans were on average, and then keeps it. pgJDBC prepares a statement on the server from its fifth execution (`prepareThreshold`), so a statement your Java service runs often can easily end up on the generic plan. That plan may suit most values and be terrible for a few, and you will never see it in psql.

`--params` shows you what the application gets.

```sh
explainsql -d shop -c "SELECT * FROM orders WHERE customer_id = ? ORDER BY created_at DESC LIMIT ?" --params --print
explainsql -d shop -f latest_orders.sql --params --measure --print
explainsql -d shop -f latest_orders.sql --bind 1=4242 --bind 2=20 --measure --print
```

## How it works

ExplainSQL prepares the statement as the application does, then:

1. **Maps each parameter** to what the statement does with it: the column it is compared with in a scan's conditions (with `=`, a range or an `IN` list), or the `LIMIT` or `OFFSET` it counts rows for.
2. **Picks values to try**, from your data:
   - for equality: the most common values, the least common of those, and a value outside them, from `pg_stats` (for a partition, from the partitioned table's statistics);
   - for a range: bounds from across the histogram;
   - for a `LIMIT`: 1, 10, 100, 1,000 and 10,000 rows;
   - for an `OFFSET`: 0, 1,000 and 100,000.
3. **Tries the values one parameter at a time**, holding the others at a typical value (the most common value, a range that keeps every row, a page of 10 rows, or the first page), and compares the custom plan for each value with the generic plan.
4. **With `--measure`, runs both plans** for each value whose custom plan differs, `--runs N` times each, after one run that warms the cache.

## The verdict

| Verdict | What it means |
|---|---|
| `INSENSITIVE` | Every value gets the generic plan. Whichever plan PostgreSQL uses, it is the same. |
| `SENSITIVE` | Some values get another plan. Measured, for at least one of them the generic plan reads at least twice as many pages (or, for as many pages, takes twice as long), or runs past the timeout. Estimated only, the planner prefers another plan for those values, and `--measure` tells you how much that matters. |
| `HARMLESS` | Some values get another plan, but measured, the generic plan does less than twice as badly for them. |
| `UNKNOWN` | A parameter has no value to try: it is compared with an expression rather than a column, or its column has no statistics. Give it one with `--bind N=VALUE`. |

Here is a real one, for a customer's latest orders:

```text
Parameters  SENSITIVE
  The plan depends on the values: with $1 = 8468, $2 = 10000, the generic plan does worse than the
  custom plan: pages 2,474 → 200,985 (81× more), execution 10.2 ms → 55.0 ms (5.4× slower).

  $1  integer, compared with orders.customer_id (=), held at 8468
  $2  bigint, the LIMIT, held at 10

  $1 = 8468   most common, 0.023% of rows
              the generic plan
  …
  $2 = 100    row count
              Parallel Seq Scan on orders, cost 4,891
              measured, custom plan → generic plan: pages 2,474 → 200,985 (81× more), execution 11.7
              ms → 68.6 ms (5.9× slower); the generic plan does worse
```

The generic plan walks the index on `created_at` backwards, hoping to find the customer's rows early. For a small `LIMIT` that works; for a larger one it reads most of the table.

## Will PostgreSQL switch?

The report also predicts whether PostgreSQL would switch to the generic plan after five executions. It switches when the generic plan's estimated cost is below the average cost of the custom plans so far, each with a charge for planning. So the outcome can depend on which values the first five executions happen to have, and the report says when it does.

When PostgreSQL could switch and the generic plan does badly, the advice is to plan every execution for its own values:

- set `plan_cache_mode = force_custom_plan` for the application's connections (in a JDBC URL, `options=-c%20plan_cache_mode=force_custom_plan`) or for its role;
- or keep the driver from preparing the statement on the server: `prepareThreshold=0` in pgJDBC, for the connection or for one statement, or `prepare_threshold = None` in psycopg 3.

Either way, each execution is planned again, which costs planning time. An index that serves every value well fixes the cause instead, and the report's advice may suggest one.

## Details worth knowing

**The plan in the report.** With `--measure`, it is the generic plan run with the values it does worst with, and the findings and advice are about that plan. Without `--measure`, it is the generic plan, estimated with the typical values.

**Giving values.** `--bind N=VALUE` sets the value of `$N`, which is then the only value tried for it (and implies `--params`). Give a value for every parameter to compare the custom and generic plans for exactly those values, such as one call taken from a log. [`explainsql logs`](logs.md) prints such a command for statements it saw switch to a generic plan.

**Placeholders.**

- `$1`, `$2`, … are read as PostgreSQL and pg_stat_statements write them.
- JDBC's `?` is converted in order when the statement has no `$n`, and `??` becomes the `?` operator.
- A statement with `$n` placeholders cannot run without values, so it needs `--params` or `--bind`.

**Safety.** Every run is rolled back, as in [connected mode](connected.md), and every statement ExplainSQL prepares is deallocated afterwards, whatever happens. `--params` needs PostgreSQL 12 or later, for `plan_cache_mode`. It prints a report; the viewer does not show it yet.

**Limits.**

- Values are tried one parameter at a time, so the way columns depend on each other is not taken into account.
- A parameter inside an expression (`lower(email) = $1`), an array (`= ANY($1)`) or a `SET` clause gets no value from the statistics. Give one with `--bind`.
