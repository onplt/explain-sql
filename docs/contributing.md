# Contributing

Thank you for wanting to help. This page explains how the project is put together, how to build and test it, and how to make the most common kinds of change. The [architecture](ARCHITECTURE.md) goes deeper into the design.

## The shape of the code

ExplainSQL is a Rust workspace with four crates:

| Crate | What it holds |
|---|---|
| `explainsql-core` | Everything that does not need I/O: the plan IR, the parsers, the metrics engine, the rules, the index advisor, diffs, checks, logs and requests analysis, and the reports. It is synchronous and pure, so it can be tested quickly and compiled to WebAssembly later. |
| `explainsql-db` | The database side: libpq-compatible connection settings and TLS, the safe executor, prepared statements, catalog and statistics reads, locks, writes, and the HypoPG and rollback provers. |
| `explainsql-tui` | The viewer, built on Ratatui and Crossterm, and the `top` list. |
| `explainsql` | The binary: the command line, connected mode, and the `check`, `logs`, `top` and `requests` commands. |

Besides the crates, `fixtures/` holds the test corpus, `xtask/` the project's own tasks, `fuzz/` the fuzz targets, `tools/cross-check/` a comparison with other tools, `install/` the install scripts and release packaging, `action/` the GitHub Action's scripts, and `docs/` this site.

## Build and test

You need Rust 1.85 or later.

```sh
cargo build                           # the debug binary, in target/debug/explainsql
cargo run -p explainsql -- --demo     # the viewer on the sample plan
cargo test --workspace                # every test
cargo fmt --all                       # formatting
cargo clippy --workspace --all-targets -- -D warnings
```

CI runs formatting, clippy with warnings as errors, actionlint and shellcheck on the workflows and scripts, the tests on Linux, macOS and Windows, the connected-mode tests against PostgreSQL 16, and a `cargo publish --dry-run` of every crate. Run the first three locally before you push, and you will rarely be surprised.

## The test database

Most tests work on captured plans and need nothing else. The tests of connected mode run against a real database named by `EXPLAINSQL_TEST_DATABASE_URL`, and are skipped when it is not set. The database needs the fixture schema, and for the full set, HypoPG and pg_stat_statements:

```sh
createdb explainsql
psql -d explainsql -v ON_ERROR_STOP=1 -f fixtures/schema.sql
psql -d explainsql -c "CREATE EXTENSION hypopg"               # postgresql-16-hypopg on Debian and Ubuntu
psql -d explainsql -c "CREATE EXTENSION pg_stat_statements"   # with shared_preload_libraries = 'pg_stat_statements'
psql -d explainsql -c "CREATE ROLE explainsql_reader LOGIN PASSWORD 'reader'"

export EXPLAINSQL_TEST_DATABASE_URL=postgresql://postgres@localhost/explainsql
export EXPLAINSQL_TEST_READER_URL=postgresql://explainsql_reader:reader@localhost/explainsql
cargo test -p explainsql-db -p explainsql -- --test-threads=1
```

The reader role has no `pg_read_all_stats`, which lets the tests check how `top` handles statements it is not allowed to see. Run these tests one at a time: building an index in one test waits for the locks of another test's rolled-back update.

## The fixture corpus

`fixtures/` holds real `EXPLAIN` output from PostgreSQL 12 to 18: 65 scenarios, each captured in JSON and text on every version. The parsers, metrics, rules and advisor are all tested against it. Each scenario is one SQL file whose header says what the plan demonstrates, which rules must fire (and no others may), and what the advisor must conclude. The [fixture README](https://github.com/onplt/explain-sql/blob/main/fixtures/README.md) describes the format.

Regenerating the corpus needs Docker:

```sh
cargo xtask gen-fixtures                               # every version, every scenario
cargo xtask gen-fixtures --versions 18 --only seq_scan_selective
cargo xtask check-fixtures                             # also part of cargo test
```

Plans in `fixtures/pg/` are never edited by hand.

## Adding or changing a rule

Rules are the easiest way to contribute, by design: one rule is one file, a scenario and a page.

1. Write the rule in `crates/explainsql-core/src/rules/esNNN_name.rs`, with its thresholds as named constants, and register it in `rules/mod.rs`. Say in the code when it deliberately stays silent: precision matters more than recall.
2. Add a scenario in `fixtures/scenarios/` whose plan triggers it, and regenerate that scenario on every version. Check that no other scenario starts firing it by mistake: `cargo test` will tell you.
3. Write its page in `docs/rules/ESNNN.md`, following the others (signal, evidence, action, when it stays silent), add it to the [catalog](rules.md) and to `docs/SUMMARY.md`, then run `cargo xtask rule-docs` to fill in the example from the corpus.

## The documentation

This site is built with [mdBook](https://rust-lang.github.io/mdBook/) from `docs/`:

```sh
mdbook build docs            # into target/book
mdbook serve docs --open     # with live reload
cargo xtask rule-docs        # refresh the examples on the rule pages
cargo xtask check-links      # every relative link and anchor resolves
```

Pages inside `docs/` must not link outside it, because the site does not contain the rest of the repository; link to the file on GitHub instead. The `Docs` workflow checks the rule pages, the demo and the links, and builds the site on every push. It deploys to GitHub Pages when run by hand or on a release tag.

When you change behavior, update the guide chapter that describes it, the [command-line reference](reference.md) if an option changed, and the changelog.

## The demo

The animated demo at the top of the README is a recording of a real session, not a mock-up. `cargo xtask demo --record` builds the release binary and runs it in a tmux pane against the database named by `EXPLAINSQL_TEST_DATABASE_URL`. It types the query and a scripted sequence of keys, waits for each screen, and saves every screen with its colors to `xtask/demo/recording.json`. The database needs the fixture schema without HypoPG, because the demo shows the index being built and measured in a rolled-back transaction.

`cargo xtask demo` then draws `docs/demo.svg` from that recording. Drawing needs no database and always gives the same SVG, so CI checks that the SVG is up to date with `cargo xtask demo --check`. The steps live in `xtask/src/demo.rs`; when the viewer's screens change, record again.

## Fuzzing

The parsers are fuzzed with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz), which needs a nightly toolchain:

```sh
mkdir -p fuzz/corpus/parse
cargo +nightly fuzz run parse fuzz/corpus/parse fixtures/pg/* fixtures/inputs
cargo +nightly fuzz run analyze fuzz/corpus/analyze fixtures/pg/* fixtures/inputs
```

`parse` runs the parsers; `analyze` also runs the analysis and every report. Neither may ever panic.

## Comparing with other tools

`tools/cross-check/` compares ExplainSQL's exclusive times with those of pev2 and explain.depesz.com on 24 reference plans. See its [README](https://github.com/onplt/explain-sql/blob/main/tools/cross-check/README.md).

## Releases

Releases are made by pushing a version tag. The release workflow builds every target, smoke-tests each archive on a clean runner, and publishes the GitHub release, the four crates on crates.io and the documentation site. Feature pull requests do not bump the version. [RELEASING.md](https://github.com/onplt/explain-sql/blob/main/RELEASING.md) has the steps.

## Licensing

Contributions are dual licensed under MIT and Apache-2.0, like the project, unless you explicitly state otherwise.
