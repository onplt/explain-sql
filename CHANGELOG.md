# Changelog

All notable changes to ExplainSQL. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Ask the planner why it chose its plan: `--why-not [TABLE]` in connected
  mode, or `y` on a node in the viewer. The statement is planned again with
  the choice taken away (`enable_seqscan = off`, `enable_nestloop = off`) or
  with enough `work_mem` for a spill, and the plans are compared. The answer
  says whether an index can serve the condition at all and what keeps it
  out, how much more expensive the planner estimates the alternative, and,
  with `--measure`, whether the planner is right, or wrong because of a row
  misestimate or its cost settings. A cost setting such as
  `random_page_cost = 1.1` is suggested only once the plan it leads to is
  measured better too. An existing index the planner did not use gets the
  reason found instead of the likely ones.
- `--measure` and `--runs N`: measure each plan N times, after one run that
  only warms the cache, and compare the medians.
- Comparisons report pages written to temporary files, and the JSON report
  says how two plans compare (`change`, `basis`).

### Changed

- Before and after comparisons lead with pages, then temporary files, then
  time, and ignore differences under 10% (and 0.1 ms). Fewer pages but a
  slower run is a mixed result.

### Fixed

- Measuring a suggested index favored the index: the run without it often
  met a colder cache than the run with it, which followed the build that
  read the whole table, and any faster run counted as better. Both sides now
  run once first to warm the cache, pages decide before time, and a
  suggestion that is not better by more than the noise drops to low
  confidence.

## [0.1.0] - 2026-10-05

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
- Prebuilt binaries for Linux, macOS and Windows with install scripts, and
  the crates on crates.io (`cargo install explainsql`).
