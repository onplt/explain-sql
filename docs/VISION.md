# Vision

## The problem

`EXPLAIN (ANALYZE, BUFFERS)` is the most important tool for understanding a slow PostgreSQL query, and its output is hard to read. A real-world plan can contain hundreds of nodes, and the numbers are easy to misread: times are per-loop averages, buffers are totals, and parallel workers and CTEs distort simple arithmetic. The root cause is often not where the biggest number is.

Today, developers typically copy the plan into a web visualizer. That breaks a terminal-centric workflow, and the visualizer can only show the plan. It cannot look at your schema, your statistics or your existing indexes, and it cannot check whether a proposed fix actually helps.

## Positioning

Plan visualization is a solved problem, and it is solved well. pev2 runs entirely in the browser and can even be used offline as a single HTML file. pgcli, vizgres, IDEs and the VS Code PostgreSQL extension all render plan trees. ExplainSQL does not try to draw a better picture of the plan.

ExplainSQL is a terminal tool that goes from **plan → diagnosis → fix → proof**:

> Pictures show you the plan. ExplainSQL tells you why it is slow, writes the fix, and proves it worked.

The structural advantage of a local tool is that it can talk to your database. A web page cannot, and a chat assistant cannot measure anything. Everything in this project follows from using that advantage carefully.

## The core feature: the proof loop

1. Open a plan from a file, a pipe or psql, or by running a query against a database.
2. The first screen answers the question: total time, the node responsible, and a one-line reason. For example: *"1.84 s. 94% of the time is in `Seq Scan on orders`: 5,000,000 rows read, 12 returned."*
3. Ask for advice and get a candidate such as `CREATE INDEX CONCURRENTLY ON orders (customer_id, status, created_at)`, with its evidence (observed selectivity, share of runtime, which columns are equality, sort or range) and a confidence level.
4. Test it:
   - If HypoPG is installed, run a hypothetical `EXPLAIN`: estimated cost, no locks, no writes. The result is labeled **estimated**.
   - Otherwise, only with an explicit `--allow-ddl` flag and after a confirmation that shows the table size, run `BEGIN; SET LOCAL lock_timeout = '2s'; CREATE INDEX …; EXPLAIN (ANALYZE, BUFFERS) …; ROLLBACK;`. The result is labeled **measured**.
5. See a before/after comparison, for example *"1.84 s → 3.2 ms · shared buffers 412k → 18 · Seq Scan → Index Scan"*, and copy the DDL.

Principles:

- **Buffers before time.** Timing depends on the state of the cache, while buffer counts are far more stable. Comparisons lead with buffers and can use the median of several runs.
- **Estimated and measured results are never mixed.** The UI always says which one you are looking at.
- **The rollback path is for development and staging databases only.** `CREATE INDEX CONCURRENTLY` cannot run inside a transaction, so verification uses a plain `CREATE INDEX`, which holds a lock that blocks writes while it runs. It is off by default.
- **HypoPG is detected, never required.** It is available on Amazon RDS (13.11+, 14.8+, 15.3+) and common on other managed services, but the tool must be fully useful without it.

## Staying in the workflow: psql pager mode

```sh
export PSQL_PAGER='explainsql --pager'
# then, inside psql:
\pset pager always
```

With this setup nothing about your psql workflow changes, but every `EXPLAIN` result opens interactively. All other output is passed through to your regular pager (`less -S`, `pspg`, and so on). This matters because an EXPLAIN tool is used occasionally. Meeting users where they already work is how it stays installed.

## Prior art and how we differ

This project builds on a lot of excellent work, and we want to be precise about what is new.

| Tool | Where it runs | Advice | Verifies advice | Notes |
|---|---|---|---|---|
| pev2 / explain.dalibo.com | Browser (also offline as one HTML file) | Limited | No | The reference visualizer; reads JSON and text |
| explain.depesz.com | Web, self-hostable | Highlighting | No | Very robust text parser (Pg::Explain) |
| pgMustard | Web, commercial | Yes (tips, scores) | No | |
| explain.tensor.ru | Web | Yes | No | |
| pg_flame | CLI → HTML flame graph | No | No | Last updated in 2020 |
| pgcli explain mode, vizgres | Terminal | No | No | Plan trees with timing highlights |
| DataGrip, pgAdmin, DBeaver, VS Code PostgreSQL extension | IDE | Limited | No | |
| Dexter, postgres-mcp, Supabase index_advisor | CLI / MCP / extension | Index advice | Yes, requires HypoPG | No interactive plan view |
| pt-visual-explain | CLI (MySQL) | No | — | |
| LLM chat assistants | Chat | Yes | No | No access to schema or statistics unless pasted; cannot measure |

Verifying index suggestions is not a new idea. It goes back to the "what-if" indexes of Microsoft's AutoAdmin research, and Dexter and postgres-mcp do it today with HypoPG. What is new here is the combination: an interactive plan explorer in the terminal, correct per-node metrics, conservative schema-aware advice, verification with or without HypoPG, and a before/after diff, while still working fully offline on a pasted plan.

## Risks we design against

| Risk | What it looks like | Mitigation |
|---|---|---|
| Becoming a worse pev2 | "Nice, but I'll keep using the web visualizer." | Treat visualization as table stakes; invest in diagnosis, verification and workflow integration |
| Parse failures on real plans | The first plan a user tries fails to parse, and they never come back | Lenient parsing; render whatever was understood along with a warning, never fail hard; `--debug-parse`; one-command anonymized bug reports that become test fixtures |
| Bad index advice | Suggesting an index on a tiny table or a low-selectivity column | Precision over recall: negative rules, confidence levels, visible evidence, and no suggestion at all when unsure |
| Scope creep | Many engines and features, none of them solid | A strict v0.1 scope (PostgreSQL only) and an explicit out-of-scope list in the [roadmap](ROADMAP.md) |
| Occasional use | Opened a few times a year, then forgotten | psql pager mode from the start; CI checks and a pg_stat_statements entry point later |
| "I just paste it into an LLM" | No reason to install a tool | Deterministic math, access to schema and statistics, measured proof; later, an MCP mode so AI agents can use the same engine |
| Single-maintainer burnout | Issues pile up | Make rules the contribution surface: one rule is one file, its fixtures and a doc page |

## Non-goals

- Being a database client. There will be no SQL editor, schema browser or query history; pgcli, Harlequin and rainfrog already do this well.
- Hosting or sharing plans. Plans stay on your machine.
- Supporting every engine on day one. PostgreSQL comes first and MySQL later.
- AI-generated explanations in the core. The engine is deterministic; integrations with AI tools are layered on top of it.
