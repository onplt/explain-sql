# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove it works, without leaving the terminal.**

> 🚧 **Design phase.** There is nothing to install yet. This repository currently holds the design documents. Watch the repository if you want to know when the first release ships.

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

## Status

Pre-alpha, design phase. Feedback on the design is welcome in the issues.
