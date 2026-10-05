# explainsql-db

Connected mode for [ExplainSQL](https://github.com/onplt/explain-sql), a terminal tool that finds out why a PostgreSQL query is slow, suggests a fix and proves it works.

This crate connects to PostgreSQL with libpq's conventions (URLs, `key=value` settings, the service file, the `PG*` variables, `~/.pgpass`, TLS through rustls). It provides:

- `EXPLAIN` that runs safely: always inside a transaction that is rolled back, with a `statement_timeout`. It is `READ ONLY` unless data-modifying statements are explicitly allowed;
- catalog reads for the tables of a plan;
- tests of a suggested index, with HypoPG or built inside a rolled-back transaction.

To use ExplainSQL itself, install the `explainsql` crate: `cargo install explainsql --locked`.

Licensed under either of Apache License, Version 2.0 or MIT license, at your option.
