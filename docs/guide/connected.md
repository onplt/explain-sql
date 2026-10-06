# Connected mode

A plan on its own can only tell you so much. With a connection, ExplainSQL can run the statement itself, check its advice against your catalog, test a suggested index before anyone creates it, and answer the questions in the next few chapters. This chapter covers how it connects, what it promises when it runs your statement, and how it tests suggestions.

```sh
explainsql -d "$DATABASE_URL" -f slow.sql
explainsql -d shop -c "SELECT * FROM orders WHERE customer_id = 42"
explainsql -d shop -f slow.sql --print          # a report instead of the viewer
```

## Connecting

`-d` takes what psql takes:

- a URL: `postgresql://app@db.internal:5432/shop?sslmode=verify-full`;
- `key=value` settings: `"host=db.internal dbname=shop user=app"`;
- or just a database name: `shop`.

Anything `-d` leaves out comes from the same places libpq looks, in the same order: the service file (`PGSERVICE`, `~/.pg_service.conf`, then the system file), the `PG*` environment variables (`PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGSSLMODE` and the rest), and finally the defaults: the local Unix socket or `localhost`, port 5432, and your user name.

Passwords come from `~/.pgpass` (`%APPDATA%\postgresql\pgpass.conf` on Windows). Like libpq, ExplainSQL ignores that file when other users can read it.

TLS follows libpq's `sslmode`:

| `sslmode` | What it does |
|---|---|
| `disable`, `allow` | No TLS. |
| `prefer` (the default), `require` | Encrypt, without checking the server's certificate. |
| `verify-ca` | Also check the certificate chain, against `sslrootcert` or the system's trust store. |
| `verify-full` | Also check that the certificate matches the host name. |

## What ExplainSQL promises when it runs your statement

This is the heart of connected mode, so it is worth reading once:

- **Every run happens inside a transaction that is rolled back**, with a `statement_timeout` (`--timeout`, 30 seconds by default). The code has no path that commits.
- **Statements that modify data or lock rows run only with `--allow-dml`**: `INSERT`, `UPDATE`, `DELETE`, `MERGE`, and `SELECT … FOR UPDATE` or `FOR SHARE`, including inside a `WITH`. ExplainSQL finds out from the estimated plan, which it gets first: a `ModifyTable` or `LockRows` node means the statement writes or locks.
- **Everything else runs in a `READ ONLY` transaction**, so even a function that writes behind your back fails.
- **One statement at a time, and only kinds that `EXPLAIN` accepts.** A statement must start with `SELECT`, `WITH`, `VALUES`, `TABLE`, `INSERT`, `UPDATE`, `DELETE` or `MERGE`. DDL, `CREATE TABLE AS` and a pasted `EXPLAIN` are refused before anything runs, and the extended query protocol refuses several statements in one string.

A rollback cannot undo everything, and you should know what slips through:

- sequences keep the values they handed out;
- dblink calls and foreign data wrappers reach other systems, which have no idea about your rollback;
- rolled-back rows leave dead tuples behind until the next `VACUUM`;
- an `UPDATE` or `DELETE` holds its row locks until the rollback, so other sessions writing the same rows wait for it.

Use `--allow-dml` on development and staging databases, or on production only when you understand those effects.

`--no-analyze` shows the estimated plan only, without running the statement at all.

## What the connection adds to the advice

Offline, every suggestion is labeled as unchecked: ExplainSQL cannot know your existing indexes, your collations or your write load. Connected, it reads the catalog for the tables in the plan, in a read-only transaction, and refines the advice:

- a suggested index that an existing valid index already covers becomes an explanation of why the planner probably did not use the existing one;
- a slow foreign-key check becomes a `CREATE INDEX` on the constraint's referencing columns;
- `text_pattern_ops` is dropped for columns with the C collation, and the `pg_trgm` caveat is dropped when the extension is installed;
- each suggestion says how large the table is, so you know what building the index will cost.

## Test a suggested index

A suggestion is a guess until it is measured. In the viewer, select a suggested index (press `i`, then `Tab` to the list if there are several) and press `t`. For a report, add `--prove`:

```sh
explainsql -d shop -f slow.sql --print --prove
explainsql -d shop -f slow.sql --print --prove --allow-ddl --runs 5
```

ExplainSQL tests it in one of two ways.

**With [HypoPG](https://github.com/HypoPG/hypopg).** If the extension is installed in the database (`CREATE EXTENSION hypopg`), ExplainSQL creates a hypothetical index in a read-only transaction and asks the planner for the plan with it. Nothing is built and nothing is locked, so it is safe anywhere and instant. The result is an estimate: the planner's opinion of the plan, not a measurement. HypoPG is available on Amazon RDS and many other managed services.

**By building it, with `--allow-ddl`.** Without HypoPG, ExplainSQL can build the index for real inside a transaction that is rolled back, run the statement with `EXPLAIN ANALYZE`, and measure the difference. `CREATE INDEX CONCURRENTLY` cannot run inside a transaction, so this uses a plain `CREATE INDEX`, which blocks writes to the table while it builds. The viewer therefore shows the table's size and asks before it starts, and the build gives up if it waits more than 2 seconds for its lock (`lock_timeout`). This is meant for development and staging databases.

Either way, you get before and after:

```text
Before → after: Pages 2,420 → 15 (161× fewer), execution 9.98 ms → 0.061 ms (164× faster)
The plan uses: orders_customer_id_idx
Verification: measured with the index created and rolled back
```

How the comparison decides:

- **Pages decide first.** Unlike times, they do not depend on what the cache happens to hold.
- **Then pages written to temporary files**, for sorts and hashes that spill.
- **Then time**, or the estimated cost when nothing was run, and only for a change of more than 10% and more than 0.1 ms. Fewer pages but a slower run is reported as mixed.
- **Each measured side runs once first**, only to warm the cache. Without that, the run without the index would often meet a colder cache than the run with it, which comes right after a build that has just read the whole table.
- **`--runs N`** measures each side N times and compares the medians.

A suggestion that the planner does not use, or that does not make the statement better by more than the noise, drops to low confidence and says so. That is the point of the test: the advice you end up with has been checked against your data.

When you are happy with a result, press `c` to copy the `CREATE INDEX CONCURRENTLY` statement, and build it the usual way.
