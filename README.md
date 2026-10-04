# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove it works, without leaving the terminal.**

> 🚧 **Early development.** There is no release yet. The repository holds the design documents, a corpus of real `EXPLAIN` plans from PostgreSQL 12–18, the parsers, and the analysis: exclusive times, twelve rules and a static report. The interactive viewer, the index advisor and the connected mode are not built yet. Watch the repository if you want to know when the first release ships.

## What it will do

ExplainSQL is a planned keyboard-driven terminal tool for reading `EXPLAIN (ANALYZE, BUFFERS)` output from PostgreSQL. Instead of only drawing the plan tree, it aims to close the loop:

1. **Diagnose.** Compute exclusive time and exclusive buffers for every node, including the parallel-query, CTE and trigger cases where simple subtraction gives the wrong answer, and open on a one-line verdict: where the time went and why.
2. **Suggest.** Flag known red flags (selective sequential scans, row misestimates, sorts and hashes spilling to disk, expensive nested loops, slow foreign-key triggers, and more) and generate `CREATE INDEX CONCURRENTLY` candidates, each with its evidence and a confidence level.
3. **Prove.** When connected to a database, test a suggested index (with HypoPG if it is installed, otherwise, only when you opt in, inside a transaction that is always rolled back) and show a before/after comparison of buffers and timing.

It is planned to work in three ways, all landing on the same screen:

- **Offline:** open or pipe a plan (JSON or text). No credentials, no network.
- **As a psql pager:** set `PSQL_PAGER` and every `EXPLAIN` you run in psql becomes interactive, while all other output goes to your usual pager.
- **Connected:** point it at a database and a `.sql` file. It runs the plan safely and unlocks the "prove" step.

PostgreSQL comes first. MySQL is on the roadmap but out of scope for the first release.

## Design documents

- [Vision](docs/VISION.md): the problem, positioning, how this differs from existing tools, risks and non-goals.
- [Architecture](docs/ARCHITECTURE.md): technology choice, crate layout, plan IR, parsing pipeline, metrics math and the index advisor design.
- [Roadmap](docs/ROADMAP.md): v0.1 scope, user experience, development phases with exit criteria, and what comes after.
- [Rule catalog](docs/rules.md): the planned red-flag rules (ES001–ES012).

## Development

```sh
cargo test --workspace                              # all tests, including the corpus checks
cargo run -p explainsql -- plan.txt                 # analyze a plan: where the time went, and what to do
cargo run -p explainsql -- --format md plan.txt     # the same report as Markdown (or --format json)
cargo run -p explainsql -- --debug-parse plan.txt   # show what the parser made of a plan
cargo xtask gen-fixtures                            # regenerate the EXPLAIN corpus (requires Docker)
```

The plan can come from a file or standard input, in JSON or text. It can be as `EXPLAIN` printed it, or still wrapped in psql output, a server log entry, cells copied from a GUI client or a Markdown code fence. For the most useful report, capture it with `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`.

The parsers are fuzzed with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz), which needs a nightly toolchain:

```sh
mkdir -p fuzz/corpus/parse
cargo +nightly fuzz run parse fuzz/corpus/parse fixtures/pg/* fixtures/inputs
```

The plan corpus and its scenario format are described in [fixtures/README.md](fixtures/README.md).

## Status

Pre-alpha. Phase 0 (workspace skeleton, CI and the fixture corpus) and Phase 1 (the parsers) are done. Phase 2 (the metrics engine, the rules and the static report) is code-complete. Phase 3 (the interactive viewer) is next. Feedback is welcome in the issues.
