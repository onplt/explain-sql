# User guide

This guide has one chapter per thing ExplainSQL does. You do not need to read it in order: [getting started](getting-started.md) covers the basics, and every chapter after that stands on its own.

**Reading plans**

- [The viewer](guide/viewer.md): the screen, every key, folding, search, the icicle view and colors.
- [As psql's pager](guide/pager.md): open every `EXPLAIN` from psql in the viewer.
- [Reports and output](guide/reports.md): text, Markdown and JSON reports, exit codes, and `--debug-parse`.

**Working against a database**

- [Connected mode](guide/connected.md): running statements safely, connection settings, and testing a suggested index.
- [Ask the planner why](guide/why-not.md): why it chose a sequential scan, a nested loop or a spilling sort, and whether it was right.
- [Statements with parameters](guide/parameters.md): custom and generic plans, and the values that make a prepared statement slow.
- [What a statement locks](guide/locks.md): relation locks, the fast path, unused indexes, and the migrations that would wait.
- [What a write costs](guide/writes.md): HOT updates, the indexes that prevent them, index entries and WAL.

**Finding the queries worth looking at**

- [The costliest statements](guide/top.md): `explainsql top`, on top of pg_stat_statements.
- [Plan changes in server logs](guide/logs.md): `explainsql logs`, on top of auto_explain.
- [N+1 loops in requests](guide/requests.md): `explainsql requests`, on top of statement logging.

**Working as a team**

- [Compare two plans](guide/diff.md): `explainsql diff`, node by node.
- [Check plans in CI](guide/ci.md): `explainsql check`, locked plans, SARIF, and the GitHub Action.
- [Share a plan](guide/anonymize.md): `explainsql anonymize`, to paste a plan anywhere without your schema and data.

Every option is also listed in the [command-line reference](reference.md).
