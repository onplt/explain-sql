# The costliest statements

Sometimes you do not have a slow query yet, just a database that feels busy. [pg_stat_statements](https://www.postgresql.org/docs/current/pgstatstatements.html) already knows which statements cost the most. `explainsql top` turns that list into a starting point: pick a statement and see its plan, without copying anything around.

```sh
explainsql top -d shop
explainsql top -d shop --limit 50 --print
explainsql top -d shop --format json > statements.json
```

## The list

`top` lists the statements that took the most execution time in the current database:

```text
The 5 costliest statements in app@db.internal:5432/shop, PostgreSQL 16.15, by total execution time

#     Total   Share  Calls      Mean   Pages  Temp  Statement
1  131.1 ms     63%      5   26.2 ms  13,130        select c.name, count(*) from customers c join o…
2   74.3 ms     36%      5   14.9 ms  12,085        select count(*) from orders where customer_id =…
3  0.513 ms   0.25%      5  0.103 ms      35        select * from orders where status=$1 limit $2
```

For each one: its total time and share of all statements' time, the number of calls, the mean time, pages read from the cache or from disk, and pages written to temporary files.

## Picking a statement

In a terminal, the list is interactive:

- **`Enter`** opens the statement's plan in the viewer, estimated: nothing runs. pg_stat_statements replaces the constants of a statement with `$1`, `$2` and so on. From PostgreSQL 16, such a statement gets its generic plan, the plan made for any value (`EXPLAIN (GENERIC_PLAN)`).
- **`p`** tries values for its parameters, exactly as [`--params`](parameters.md) does, and shows the report. Before PostgreSQL 16, `Enter` does this too, since there is no other way to plan a statement with parameters.
- **`q`** goes back from the viewer to the list, and quits from the list.

Only with `--measure` do the plans actually run, in a transaction that is rolled back, as in [connected mode](connected.md). `--allow-dml` lets it measure statements that write, too.

Outside a terminal, or with `--print`, the list is printed as text, Markdown or JSON (`--format`).

## Statements it cannot plan

The list marks those, and says why:

- commands without a plan, such as `VACUUM`, `SET` or `EXPLAIN`, ExplainSQL's own included;
- other users' statements, whose text pg_stat_statements shows only to superusers and members of `pg_read_all_stats`;
- texts cut short at `track_activity_query_size` bytes (1024 by default).

## Requirements

pg_stat_statements must be loaded when the server starts and created in the database:

```text
shared_preload_libraries = 'pg_stat_statements'    # in postgresql.conf, then restart
```

```sql
CREATE EXTENSION pg_stat_statements;
```

ExplainSQL tells you which of the two is missing. The list holds the current database's statements. From PostgreSQL 14, it holds only those the application sent: a statement run inside a function counts in the call to that function. Reading the list happens in a `READ ONLY` transaction that is rolled back.

## Limits

The text pg_stat_statements keeps does not always parse again. A constant written with its type, such as `timestamptz '2026-01-01'`, becomes `timestamptz $1`, which PostgreSQL refuses. Planning such a statement shows the server's error; put the constant back and run the statement with `explainsql -d … -c`.
