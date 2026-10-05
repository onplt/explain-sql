# explainsql-core

The engine of [ExplainSQL](https://github.com/onplt/explain-sql), a terminal tool that finds out why a PostgreSQL query is slow, suggests a fix and proves it works.

This crate performs no I/O and holds no async code, so it also builds for WebAssembly. It provides:

- parsers for `EXPLAIN` plans in JSON and text from PostgreSQL 12 to 18, as EXPLAIN prints them or still wrapped in psql output, server logs, GUI client cells or Markdown fences;
- exclusive time and buffers for every node, including parallel query, CTEs, InitPlans, SubPlans and triggers;
- twelve rules (ES001–ES012) that turn plans into findings with evidence and an action;
- an index advisor that writes `CREATE INDEX CONCURRENTLY` candidates;
- text, Markdown and JSON reports.

```rust
let plan = explainsql_core::parse(
    "Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.2..12.6 rows=10 loops=1)\n  Filter: (customer_id = 4242)\n  Rows Removed by Filter: 199990",
)
.unwrap();
let analysis = explainsql_core::analyze(&plan);
println!("{}", analysis.verdict);
```

To use ExplainSQL itself, install the `explainsql` crate: `cargo install explainsql --locked`.

Licensed under either of Apache License, Version 2.0 or MIT license, at your option.
