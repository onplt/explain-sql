# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove that it works, without leaving the terminal.**

![ExplainSQL running a slow query against PostgreSQL: the verdict, the slowest node, why the planner uses no index, the suggested index measured before and after in a rolled-back transaction, and the locks the statement takes](demo.svg)

## What it is

ExplainSQL reads the plans PostgreSQL prints for `EXPLAIN (ANALYZE, BUFFERS)` and tells you what they mean. Instead of handing you a prettier tree and leaving the rest to you, it goes all the way from the plan to a fix you can trust:

1. **Diagnose.** It works out the time and pages spent in each node, including the cases where simple subtraction is wrong: parallel workers, CTEs, InitPlans and triggers. The first line on the screen is a verdict, such as *"20.0 ms. 99% of it in Seq Scan on orders o, which reads 200,000 rows to keep 10."*
2. **Suggest.** [Thirteen rules](rules.md) flag well-known problems, each with its evidence and what to do. An index advisor writes `CREATE INDEX CONCURRENTLY` statements, and when no index would help, it says why.
3. **Prove.** Connected to a database, it runs the statement in a transaction that is always rolled back, tests the suggested index with HypoPG or by building it inside that transaction, and shows before and after, pages first.

On top of that loop, it answers the questions that usually come next. Why did the planner not use my index? Is the generic plan of this prepared statement bad for some values? Which locks does this query take, and what would wait for them? Why was this update not HOT? Did a plan get worse in this pull request? When did the plan of this statement change last night? Which requests run the same query fifty times?

## How it fits into your day

You can use it in three ways, and they all lead to the same screen:

- **Offline**, on a plan you already have: a file, the clipboard, or psql's output piped in. No credentials and no network.
- **As psql's pager**: `PSQL_PAGER='explainsql --pager'`, and every `EXPLAIN` you run in psql opens in the viewer.
- **Connected**: `explainsql -d "$DATABASE_URL" -f slow.sql` runs the query safely and unlocks the proof, the planner questions, locks and writes.

It also has commands for the rest of the team's workflow: [`check`](guide/ci.md) and a GitHub Action for CI, [`diff`](guide/diff.md) to compare two plans, [`top`](guide/top.md) for pg_stat_statements, [`logs`](guide/logs.md) and [`requests`](guide/requests.md) for server logs, and [`anonymize`](guide/anonymize.md) to share a plan without giving away your schema.

## Where to go next

- New here? [Getting started](getting-started.md) takes you from install to your first proven fix in a few minutes.
- Looking for a feature? The [user guide](guide.md) has a chapter for each one.
- Need a flag? The [command-line reference](reference.md) lists them all.
- Puzzled by a finding? The [rule catalog](rules.md) explains every one, with an example from a real plan.
- Something odd? Try [troubleshooting](troubleshooting.md).
- Curious how it works, or want to help? Read the [architecture](ARCHITECTURE.md) and the [contributing guide](contributing.md).

ExplainSQL supports PostgreSQL 12 to 18. It is free software under the MIT or Apache-2.0 license, at your option.
