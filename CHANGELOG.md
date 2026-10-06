# Changelog

All notable changes to ExplainSQL. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `explainsql requests LOGS`: the statements of server logs grouped into
  requests, by the trace id of their sqlcommenter `traceparent` tag, their
  transaction or their session, and the loops in them: a statement run
  again and again in one request with another value each time (N+1), or
  with the same values. For each loop, the batched statement that does the
  work of all its runs at once (`= ANY($1)`, or a `LATERAL` subquery over
  `unnest($1)` when its rows must stay per value), and how to make the ORM
  send it. With `-d`, the batched statement and the runs are measured, each
  run rolled back, with the round trips they need, and the foreign key
  behind the loop is named. Reads statement logging
  (`log_min_duration_statement`, `log_statement` with `log_duration`) in
  stderr, csvlog and jsonlog, values from the `parameters:` detail lines, or
  auto_explain entries. See the guide.
- Server log entries carry their session (`%c`) and virtual transaction
  (`%v`), from jsonlog and csvlog fields or a stderr `log_line_prefix`; `explainsql logs --format json` shows them.
- `--locks` in connected mode, and `L` in the viewer: the locks the
  statement takes, read inside the transaction that is rolled back. How
  many fall outside the fast path, the tables, partitions and indexes they
  come from (indexes nothing uses named), the commands that would wait for
  them, other sessions' conflicting locks right now, and, from a second
  connection that samples `pg_stat_activity`, what the statement waited on
  as it ran. With `--params` or `--bind`, the locks of an execution of the
  generic plan, which locks every partition, and of a custom plan. See the
  guide.
- With `--locks`, `--measure` or `--prove`, a measured run that waited for
  another session's lock runs again, up to twice, with a note.

## [0.2.0] - 2026-10-06

### Added

- `explainsql anonymize`: a plan to share in a bug report or an issue, with
  the names of tables, indexes, columns and other objects replaced
  (`table_a`, `index_a`, `column_a`, …, partitions staying alike as
  `table_b_1`, `table_b_2`) and literal values replaced (`'value_a'`, other
  numbers), the same way everywhere they appear. Node types, figures and
  findings stay as they were, and the plan compares and folds as before.
  `--keep-names` replaces only the values; `--map FILE` writes what each
  name and value became.
- A GitHub Action (`uses: onplt/explain-sql@v0.2.0`): runs `explainsql check`
  and writes the report on the pull request as one comment, updated in place
  on later runs. See the guide.
- `explainsql check --sarif FILE` writes the SARIF report beside a report in
  another format.
- `F` in the viewer: the plan as an icicle, each node as wide as the CPU
  time in it and below it (or its estimated cost, without timing), with
  zoom. See the guide.
- `explainsql top -d DATABASE`: the statements that took the most execution
  time, from pg_stat_statements. In a terminal, Enter shows the plan of the
  one selected without running it, with `EXPLAIN (GENERIC_PLAN)` for a
  statement with parameters from PostgreSQL 16, and p tries values for its
  parameters as `--params` does. `--print` prints the list as text,
  Markdown or JSON.

### Changed

- `explainsql check --format md` starts with a hidden marker line and stays
  under GitHub's limit for a comment, leading with the plans that failed.

## [0.1.1] - 2026-10-06

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
- `explainsql diff BEFORE AFTER` compares two plans of the same statement,
  node by node: scans that read their relation another way, joins with
  another method or order, other strategies, nodes that came or went,
  spills, misestimates, and nodes whose work changed beyond the noise, the
  most significant first, then the plan after with its changes marked. It
  matches nodes by the work they do, so partitions renamed by pruning or by
  another PostgreSQL version still match. Text, Markdown or JSON.
- Plan shapes: an id for what makes a plan that plan, without its numbers,
  literal values or aliases, the same in JSON and text.
- Reading every plan of an input: plans pasted one after the other, JSON
  arrays and documents, Markdown fences, psql results and log entries
  (`parse_all`). `explainsql diff` takes both plans from one input this way.
- `explainsql check`, a gate for CI: plan files, or SQL files run against a
  database (`-d`), each checked against its findings (`--fail-on`) and
  against the plan locked for it in `explainsql.lock` (`--update` writes
  it). A plan fails when it is worse by pages, or by the estimated cost when
  not run; time alone, for the same pages, is a note. `--strict` fails any
  change of plan, and `--prove` tests the suggested fixes of the plans that
  failed. Exit codes 0, 1 and 2; text, Markdown, JSON or SARIF.
- `--fail-on SEVERITY`: with a printed report, exit with 1 when a finding is
  at least that severe.
- `explainsql logs FILES`: the plans auto_explain logged, and for each
  statement, which plans it got, when its plan changed, after how many runs
  and in which session, the median duration before and after, how the plan
  after compares and what changed, the costliest change first. Reads
  stderr logs with any line prefix, csvlog and jsonlog, plans in text or
  JSON. Statements are told apart by their query identifier, or by their
  text without literal values. A switch to a prepared statement's generic
  plan is named, with the values it ran with (PostgreSQL 16+) and the
  `--params --bind` command that tests it. sqlcommenter tags say where a
  statement comes from. `--changed`, `--query`, `--trace`, `--since` and
  `--until` narrow the report; text, Markdown or JSON.
- `parse_log`: every auto_explain entry of a server log, with its time,
  process, user, database, application, duration, query identifier and
  parameters.
- ES013, planner settings force the plan: the plan was made with an
  `enable_*` setting off, as left on in a session or set for a role or a
  database, or the planner used a node such a setting disables because it
  found no other way (`Disabled: true` from PostgreSQL 18, the disable cost
  before). Applications that plan with the defaults may get another plan.
- I/O time, from plans captured with `track_io_timing`: among the
  statement's facts when it takes a tenth of the time or more, in the
  verdict when reading pages that were not in shared buffers took half of
  it (the cache was cold), and for each node in the viewer's details. It is
  compared with the time of every process, so that parallel plans read
  right.
- `--params` in connected mode: how the plan of a statement with parameters
  (`$1`, or JDBC's `?`) depends on their values. explainsql prepares the
  statement as an application does, tries values from the columns'
  statistics and common LIMIT and OFFSET row counts, and compares the
  custom plan each value gets with the generic plan, which PostgreSQL may
  switch to after five executions. With `--measure`, both plans run where
  they differ; the report says whether the generic plan does much worse for
  some value, whether PostgreSQL would switch to it, and what to do
  (`plan_cache_mode = force_custom_plan`, pgJDBC's `prepareThreshold=0`),
  and shows the generic plan with the values it does worst with. `--bind
  N=VALUE` gives a parameter's value.

### Changed

- ES003 and ES004 name the `work_mem` a spilled sort or hash needs, from
  what the plan shows, to set for the statement alone (`SET LOCAL work_mem
  = '64MB'` in its transaction), and say what it may take: each sort, hash
  and other operation that uses `work_mem` may take that much, in each
  process that runs it, in every session that runs the statement at once.
- In connected mode, a run compared with the previous one says what changed
  in the plan, or that it is the same plan.
- Before and after comparisons lead with pages, then temporary files, then
  time, and ignore differences under 10% (and 0.1 ms). Fewer pages but a
  slower run is a mixed result.
- The README's demo is a recording of a real session: explainsql run
  against PostgreSQL on a slow query, asked why the planner uses no index,
  and testing the suggested index with HypoPG. `cargo xtask demo --record`
  records it again; `cargo xtask demo` draws it from the recording.

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
