# Summary

[Introduction](index.md)

- [Getting started](getting-started.md)

# User guide

- [Overview](guide.md)
  - [The viewer](guide/viewer.md)
  - [As psql's pager](guide/pager.md)
  - [Reports and output](guide/reports.md)
  - [Connected mode](guide/connected.md)
  - [Ask the planner why](guide/why-not.md)
  - [Statements with parameters](guide/parameters.md)
  - [What a statement locks](guide/locks.md)
  - [What a write costs](guide/writes.md)
  - [The costliest statements](guide/top.md)
  - [Plan changes in server logs](guide/logs.md)
  - [N+1 loops in requests](guide/requests.md)
  - [Compare two plans](guide/diff.md)
  - [Check plans in CI](guide/ci.md)
  - [Share a plan](guide/anonymize.md)
- [Command-line reference](reference.md)
- [Troubleshooting](troubleshooting.md)

# Rules

- [Rule catalog](rules.md)
  - [ES001: Selective sequential scan](rules/ES001.md)
  - [ES002: Row misestimate](rules/ES002.md)
  - [ES003: Sort spilled to disk](rules/ES003.md)
  - [ES004: Hash or aggregate spilled to disk](rules/ES004.md)
  - [ES005: Expensive nested-loop inner side](rules/ES005.md)
  - [ES006: Index scan that filters most rows](rules/ES006.md)
  - [ES007: Index-only scan with many heap fetches](rules/ES007.md)
  - [ES008: Lossy bitmap or heavy recheck](rules/ES008.md)
  - [ES009: Slow foreign-key trigger](rules/ES009.md)
  - [ES010: Cartesian product](rules/ES010.md)
  - [ES011: Fewer parallel workers than planned](rules/ES011.md)
  - [ES012: JIT overhead dominates](rules/ES012.md)
  - [ES013: Planner settings force the plan](rules/ES013.md)

# Design and development

- [Architecture](ARCHITECTURE.md)
- [Contributing](contributing.md)
