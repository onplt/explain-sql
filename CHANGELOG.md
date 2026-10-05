# Changelog

All notable changes to ExplainSQL. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

The first release: find out why a PostgreSQL query is slow, get a fix, and
prove it works, without leaving the terminal.

### Added

- Parsers for EXPLAIN plans in JSON and text from PostgreSQL 12 to 18, as
  EXPLAIN prints them or still wrapped in psql output, server logs, GUI
  client cells or Markdown fences.
- Exclusive time and buffers for every node, including parallel query,
  CTEs, InitPlans, SubPlans and triggers, cross-checked with pev2 and
  explain.depesz.com.
- Twelve rules (ES001–ES012) with evidence and an action for each finding.
- An index advisor: `CREATE INDEX CONCURRENTLY` candidates ordered by the
  ESR rule, query rewrites, and explanations of why a slow scan gets no
  index.
- An interactive viewer: verdict, plan tree with heat bars, details,
  findings and advice, search, folding of similar siblings, view modes,
  and a psql pager mode (`PSQL_PAGER='explainsql --pager'`).
- Static reports as text, Markdown and JSON (`--print`, `--format`).
- Connected mode (`-d`, `-f`, `-c`): libpq-compatible connection settings,
  every run rolled back, `READ ONLY` unless `--allow-dml`, catalog reads
  that refine the advice.
- A proof loop: test a suggested index with HypoPG, or build it in a
  rolled-back transaction with `--allow-ddl`, and compare before and after.
- `--demo`, a bundled example plan.
