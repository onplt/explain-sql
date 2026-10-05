# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove it works, without leaving the terminal.**

![ExplainSQL showing a plan, its findings and a suggested index](docs/demo.svg)

> **Version 0.1.0, not released yet.** The release pipeline is ready; binaries appear on the [releases page](https://github.com/onplt/explain-sql/releases) once the first version is tagged. Until then, build from source (below).

ExplainSQL reads `EXPLAIN (ANALYZE, BUFFERS)` output from PostgreSQL. Instead of only drawing the plan tree, it closes the loop:

1. **Diagnose.** It computes exclusive time and buffers for every node, including the parallel-query, CTE and trigger cases where simple subtraction gives the wrong answer. It opens on a one-line verdict: where the time went and why.
2. **Suggest.** Twelve rules flag known red flags: selective sequential scans, row misestimates, sorts and hashes spilling to disk, expensive nested loops, slow foreign-key triggers, and more. An index advisor writes `CREATE INDEX CONCURRENTLY` candidates, each with its evidence and a confidence level, and explains why a slow scan gets none.
3. **Prove.** Connected to a database, it runs the query inside a transaction that is always rolled back. It tests a suggested index with HypoPG, or, only when you opt in, by building it in a rolled-back transaction. It then shows before and after.

It works in three ways, all landing on the same screen:

- **Offline:** open or pipe a plan, in JSON or text. It can be as EXPLAIN printed it, or still wrapped in psql output, a server log or a Markdown fence. No credentials, no network.
- **As psql's pager:** with `PSQL_PAGER='explainsql --pager'`, every `EXPLAIN` you run in psql opens in the viewer, and all other output goes to your usual pager.
- **Connected:** `explainsql -d "$DATABASE_URL" -f slow.sql` runs the query safely and unlocks the "prove" step.

PostgreSQL 12 to 18 are supported. MySQL is on the roadmap but out of scope for the first release.

## Install

Once released, the install scripts download the binary for your platform, check its SHA-256 checksum, and put it in `~/.local/bin`:

```sh
curl -fsSL https://github.com/onplt/explain-sql/releases/latest/download/install.sh | sh
```

```powershell
irm https://github.com/onplt/explain-sql/releases/latest/download/install.ps1 | iex
```

Binaries are built for Linux (x86_64 and aarch64, static), macOS (Intel and Apple silicon) and Windows (x86_64). From source, with Rust 1.85 or later:

```sh
cargo install --git https://github.com/onplt/explain-sql explainsql --locked
```

Then try it on the bundled example: `explainsql --demo`.

## Use

```sh
explainsql plan.json                                   # open a plan in the viewer (press ? for the keys)
psql -XAtq -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT …" | explainsql
explainsql --print --format md plan.txt                # a report for an issue or a pull request
explainsql -d "$DATABASE_URL" -f slow.sql              # run it: estimated plan, then EXPLAIN ANALYZE, rolled back
explainsql -d "$DATABASE_URL" -f slow.sql --print --prove   # and test each suggested index
```

The [user guide](docs/guide.md) covers the viewer's keys, the pager mode, connected mode and its safety rules, and testing suggestions. The [rule catalog](docs/rules.md) explains every finding, with an example from real plans. Both are also published as the [documentation site](https://onplt.github.io/explain-sql/).

## Design documents

- [Vision](docs/VISION.md): the problem, positioning, how this differs from existing tools, risks and non-goals.
- [Architecture](docs/ARCHITECTURE.md): technology choice, crate layout, plan IR, parsing pipeline, metrics, the index advisor, connected mode and the release pipeline.
- [Roadmap](docs/ROADMAP.md): v0.1 scope, development phases with exit criteria, and what comes after.
- [Changelog](CHANGELOG.md).

## Development

```sh
cargo test --workspace            # all tests, including the corpus checks
cargo run -p explainsql -- --demo # the viewer on the sample plan
cargo xtask gen-fixtures          # regenerate the EXPLAIN corpus (requires Docker)
cargo xtask rule-docs             # refresh the examples on the rule pages
cargo xtask demo                  # redraw docs/demo.svg
mdbook build docs                 # the documentation site, in target/book
```

Tests of connected mode run against a database with the fixture schema, named by `EXPLAINSQL_TEST_DATABASE_URL`; without it they are skipped. The plan corpus and its scenario format are described in [fixtures/README.md](fixtures/README.md). The parsers are fuzzed with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz), which needs a nightly toolchain:

```sh
mkdir -p fuzz/corpus/parse
cargo +nightly fuzz run parse fuzz/corpus/parse fixtures/pg/* fixtures/inputs
```

To release, push a tag that matches the version in `Cargo.toml` (`git tag v0.1.0 && git push origin v0.1.0`). The release workflow builds every target, smoke-tests each archive on a clean runner, and publishes the GitHub release.

## Status

Pre-alpha. Phases 0 to 4 are done: the fixture corpus, the parsers, the metrics engine and rules, the viewer, the index advisor, connected mode and the proof loop. Phase 5, release hardening, has its pipeline, install scripts, documentation site and demo; the first tagged release is next. Feedback is welcome in the issues.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in ExplainSQL by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
