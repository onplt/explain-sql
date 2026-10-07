# Playground

A local PostgreSQL to try ExplainSQL against, with one command. It needs Docker with Compose.

- PostgreSQL 16 with the [fixture schema](../fixtures/schema.sql): customers, orders, order items, partitioned events and the rest, with the missing indexes, correlated columns and stale statistics the rules are tested on.
- [HypoPG](https://github.com/HypoPG/hypopg), so `t` and `--prove` test an index without building it.
- pg_stat_statements, loaded at startup and filled with a small workload, so `explainsql top` has statements to list.
- `track_io_timing` on, and autovacuum off so the statistics stay as stale as the schema leaves them.
- [Sample statements](queries/), one per feature worth trying.

## Start and stop

From the repository root:

```sh
docker compose -f playground/compose.yaml up -d --build --wait   # about a minute the first time
export DATABASE_URL='postgresql://postgres:explainsql@localhost:55432/shop?sslmode=disable'
explainsql -d "$DATABASE_URL" -f playground/queries/01-customer-summary.sql
docker compose -f playground/compose.yaml down -v                # stop and delete the data
```

In PowerShell, set the URL with `$env:DATABASE_URL = '…'` and use `$env:DATABASE_URL` in place of `"$DATABASE_URL"`.

The server listens on port 55432 so that it does not clash with a PostgreSQL you already run; set `EXPLAINSQL_PLAYGROUND_PORT` to change it. `down` without `-v` keeps the data for the next `up`.

## What to try

| Statement | Command | What you see |
|---|---|---|
| [01](queries/01-customer-summary.sql) | `explainsql -d "$DATABASE_URL" -f …/01-customer-summary.sql` | The verdict, an `ES001` finding, an index to suggest. In the viewer: `1`, `i`, `t`, `y`, `F`, `L`. |
| [02](queries/02-orders-by-status.sql) | `… -f …/02-orders-by-status.sql --params --print` | A parameter that does not matter: `INSENSITIVE`. |
| [03](queries/03-latest-orders.sql) | `… -f …/03-latest-orders.sql --params --measure --print` | JDBC `?` placeholders whose generic plan reads 80× more pages: `SENSITIVE`. |
| [04](queries/04-update-status.sql) | `… -f …/04-update-status.sql --allow-dml` | Non-HOT updates and their WAL (`W`), and the locks (`L`). Rolled back. |
| [05](queries/05-delete-orders.sql) | `… -f …/05-delete-orders.sql --allow-dml --print` | A delete whose time goes to foreign-key checks on an unindexed column. |
| [06](queries/06-day-of-orders.sql) | `… -f …/06-day-of-orders.sql --why-not --print` | A function on the column: the advice is a rewrite, not an index. |
| [07](queries/07-correlated-columns.sql) | `… -f …/07-correlated-columns.sql` | A 20× underestimate (`▲`) from correlated columns. |
| [08](queries/08-stale-statistics.sql) | `… -f …/08-stale-statistics.sql` | A 50,000× underestimate from stale statistics. |
| [09](queries/09-index-not-used.sql) | `… -f …/09-index-not-used.sql --why-not --measure --print` | An index the planner is right not to use. |
| | `explainsql top -d "$DATABASE_URL"` | The workload's costliest statements. `Enter` plans one, `p` tries its parameters. |
| | `explainsql requests fixtures/requests/postgresql.log -d "$DATABASE_URL"` | N+1 loops in a statement log, measured against this database. |
| | `explainsql logs fixtures/logs/postgresql.log` | A plan that changed in an auto_explain log. |

To test an index by building it instead of with HypoPG, drop the extension and add `--allow-ddl`; `t` then asks before it builds:

```sh
psql "$DATABASE_URL" -c "DROP EXTENSION hypopg"     # CREATE EXTENSION hypopg; brings it back
```
