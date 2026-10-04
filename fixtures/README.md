# Fixture corpus

Real `EXPLAIN` output captured from PostgreSQL 12–18. The parsers, the metrics engine, the rules and the index advisor are developed and tested against it.

```
fixtures/
├─ schema.sql               # deterministic dataset, loaded once per server
├─ scenarios/<name>.sql     # one statement plus header directives per scenario
├─ pg/<major>/
│  ├─ <name>.json           # EXPLAIN (…, FORMAT JSON)
│  ├─ <name>.txt            # EXPLAIN (…, FORMAT TEXT)
│  └─ manifest.json         # server version, JIT availability, status of every scenario
└─ inputs/                  # one plan in the forms it is pasted or logged in (see below)
```

## Regenerating

Generation needs Docker. Plans in `pg/` are never edited by hand.

```sh
cargo xtask gen-fixtures                          # every version, every scenario
cargo xtask gen-fixtures --versions 17,18         # selected versions
cargo xtask gen-fixtures --versions 18 --only seq_scan_selective
cargo xtask check-fixtures                        # also part of `cargo test`
```

For each major version the generator starts a throwaway `postgres:<major>` container with `autovacuum=off`, `track_io_timing=on`, `shared_buffers=32MB`, `fsync=off` and `synchronous_commit=off`, and loads `schema.sql`. Then, for each scenario and inside a transaction that is always rolled back, it runs a discarded warm-up (for `ANALYZE` scenarios), the JSON capture and the text capture. Scenarios that modify data run after the read-only ones, because even a rolled-back statement clears visibility map bits on the pages it touches. Scenarios the server cannot run (too old, or no JIT) are recorded as skipped in `manifest.json`.

`--only` refreshes the listed scenarios and keeps the others. Before committing, regenerate the whole corpus so that every plan comes from the same server builds.

The `Fixtures` GitHub workflow (run manually) does the same on a GitHub runner and uploads the result as an artifact.

## What is reproducible

- **The same on every run:** plan shape, node types, estimates, actual row counts and loops (except how rows are split between parallel workers).
- **Different on every run:** timings, the split between buffer hits and reads, I/O timings, parallel worker distribution.
- JSON and text come from two separate executions. They agree on plan shape, estimates and row counts, but not on timings.
- OIDs in trigger names (such as `RI_ConstraintTrigger_a_16417`) can differ between major versions.

## Captured inputs

Plans rarely arrive as clean `EXPLAIN` output: they come from psql in one of its output formats, or from a server log. `inputs/` holds a single plan in each of these forms, all captured from one PostgreSQL 16 server for the same query:

```sql
SELECT grp, count(*)
FROM t
WHERE grp < 10
GROUP BY grp
ORDER BY grp
```

| Files | Captured with |
|---|---|
| `reference.txt`, `reference.json` | `psql -X -At -c "EXPLAIN (ANALYZE, BUFFERS) …"`, and the same with `FORMAT JSON` |
| `psql-aligned.txt`, `psql-aligned-json.txt` | psql's default aligned output |
| `psql-unicode.txt`, `psql-unicode-json.txt` | `-P linestyle=unicode` |
| `psql-border2.txt` | `-P border=2` |
| `psql-wrapped.txt` | `-P format=wrapped -P columns=60` |
| `psql-expanded.txt`, `psql-expanded-json.txt` | `-x` |
| `auto_explain-text.log`, `auto_explain-json.log` | `auto_explain` (`log_analyze`, `log_buffers`, `log_min_duration = 0`) in the stderr log, with `log_format` text and json |
| `jsonlog-text.json`, `jsonlog-json.json` | the same entries in the `jsonlog` log (`log_destination = 'stderr,jsonlog'`) |

`crates/explainsql-core/tests/inputs.rs` requires every file to parse, without warnings, into the same tree as `reference.txt`, and the log entries to keep their query text. Unlike `pg/`, these files are not produced by `xtask`. To cover another form, capture it from a real server, add it here and add it to the test.

## Scenario files

```sql
-- description: Sequential scan whose filter keeps 10 of 200,000 rows because customer_id has no index.
-- rules: ES001
-- advice: index
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id = 4242;
```

The header is every leading line that starts with `--`. Everything after it is the single statement to explain.

| Directive | Required | Meaning |
|---|---|---|
| `description` | yes | What the plan demonstrates |
| `rules` | no | IDs from the [rule catalog](../docs/rules.md) that the plan is expected to trigger (at least these) |
| `advice` | no | Expected advisor outcome: `index` (a candidate), `none` (no suggestion; a trap for naive advisors) or `rewrite` (fix the query rather than the schema) |
| `min_version` | no | Oldest major version that supports the scenario (default 12) |
| `requires` | no | `jit`: capture only on servers built with JIT |
| `set` | no, repeatable | A setting applied with `SET` before the statement |
| `options` | no | EXPLAIN options without `FORMAT` (default `ANALYZE, BUFFERS, VERBOSE, SETTINGS`) |

To add a scenario, write the file, run `cargo xtask gen-fixtures` for every version, read the text plans to confirm they show what the description claims, and commit the `.sql` file together with `pg/`.

## Dataset

| Table | Rows | Deliberate properties |
|---|---|---|
| `customers` | 20,000 | |
| `products` | 5,000 | |
| `orders` | 200,000 | About 2,300 pages; `customer_id` and `status` are not indexed; index on `created_at` |
| `order_items` | 300,000 | `order_id` is not indexed (foreign-key trigger and nested-loop scenarios); orders above 190,000 have no items |
| `events` | 120,000 | 12 monthly partitions for 2025; index on `created_at`; `jsonb` payload |
| `addresses` | 200,000 | `city` determines `country` (correlated columns) |
| `page_views` | 100,000 | Updated after the last `VACUUM`, so the visibility map is out of date |
| `shipments` | 100,000 | 50,000 rows inserted after the last `ANALYZE`, so statistics are stale |
| `audit_log` | 5,000 | No indexes at all |
| `settings_kv` | 50 | A tiny lookup table |
