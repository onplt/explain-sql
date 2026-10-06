# ExplainSQL

**Find out why your PostgreSQL query is slow, get a fix, and prove it works, without leaving the terminal.**

![ExplainSQL run against PostgreSQL: the verdict on a slow query, why the planner uses no index, and the suggested index tested with HypoPG](demo.svg)

ExplainSQL reads `EXPLAIN (ANALYZE, BUFFERS)` output from PostgreSQL 12 to 18, in JSON or text, as EXPLAIN prints it or still wrapped in psql output, a server log or a Markdown fence. It then:

1. **Diagnoses.** It works out where the time went, node by node. That includes parallel workers, CTEs, InitPlans and triggers, where simple subtraction gives the wrong answer. It opens on a one-line verdict.
2. **Suggests.** Thirteen rules flag known problems, each with its evidence and what to do. An index advisor writes `CREATE INDEX CONCURRENTLY` candidates and explains why a slow scan gets none.
3. **Proves.** Connected to a database, it runs the query safely, always rolled back, and tests a suggested index. It uses HypoPG if installed, otherwise builds the index in a transaction that is rolled back. Then it shows before and after.

It works offline on a plan file, as psql's pager, or connected to a database. Start with the [user guide](guide.md). The [rule catalog](rules.md) explains every finding.
