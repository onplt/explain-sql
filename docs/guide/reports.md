# Reports and output

The viewer is for exploring. Reports are for everything else: pasting into a ticket, posting on a pull request, feeding another program, or failing a script.

## When you get a report

ExplainSQL opens the viewer only when standard output is a terminal (and `TERM` is not `dumb`). Otherwise, or whenever you pass `--print`, it prints a report:

```sh
explainsql --print plan.json
explainsql plan.json > report.txt                 # not a terminal: a report
explainsql -d shop -f slow.sql --print --prove    # some options only make sense in a report
```

`--prove`, `--why-not` and `--params` print reports; in the viewer you use `t` and `y` instead.

## Formats

**Text** (`--format text`, the default) is made for the terminal: the verdict, the statement's figures, the plan tree with each node's share, time, a bar and misestimate marks, then the findings, the advice, and any sections you asked for (why not, parameters, locks, writes). Colors are used when the output is a terminal and `NO_COLOR` is not set; `--color always` or `--color never` overrides that.

**Markdown** (`--format md`) is ready to paste into a GitHub or GitLab issue, a pull request or a wiki. The plan becomes a table, findings link to their rule's page, and suggested indexes sit in SQL code blocks.

**JSON** (`--format json`) is for other programs. It is the complete analysis, pretty-printed:

| Key | What it holds |
|---|---|
| `verdict` | The one-sentence verdict. |
| `findings` | Each finding: `rule` (`id`, `name`, and `docs`, a link to its page), `severity` (`low`, `medium` or `high`), `node` (the node's id in `plan.nodes`), `summary`, `evidence` (a list of `label` and `value`) and `action`. |
| `advice` | Each suggestion or explanation: `kind` (such as `index`), the `node`, for an index its `index` (schema, table, method, columns) and `ddl`, a `confidence` (`low`, `medium` or `high`), `summary`, `evidence`, `caveats`, and how it was verified. |
| `counterfactuals` | The answers of `--why-not`, when asked. |
| `parameters` | The `--params` analysis, when run. |
| `writes` | [What the writes cost](writes.md): per-table rows, HOT updates, blocking indexes, WAL and notes, for a statement run with `--allow-dml`. |
| `locks` | [The locks](locks.md) the statement took, per stage (planned, ran, generic or custom plan), with `--locks`. |
| `metrics` | Figures the engine derived: `statement` (total, planning and execution time, time outside the tree, I/O, hotspots) and `nodes`, indexed like `plan.nodes` (inclusive and exclusive time and CPU time, share, exclusive buffers and I/O, total rows, misestimate factor, and whether the node may stop early). |
| `plan` | The parsed plan itself: its `nodes` with their properties, the statement `summary`, and the `source` (JSON or text, and the wrappers that were removed). |

Keys that do not apply to a report are left out. Times are in milliseconds.

The other commands have their own JSON reports, described in their chapters: [`diff`](diff.md), [`check`](ci.md) (which also speaks SARIF), [`logs`](logs.md), [`top`](top.md) and [`requests`](requests.md).

## Confidence and severity

Findings have a **severity** that follows the share of the runtime involved: half or more is high, a fifth or more is medium, anything less is low. The [rule catalog](../rules.md) has the details.

Suggestions have a **confidence**, shown as `SURE`, `LIKELY` or `MAYBE` in text reports and as `high`, `medium` or `low` in JSON. A suggestion starts at high and is lowered for an inferred column, an operator class that depends on the collation, `pg_trgm`, or a comparison with a value only known at run time. One that was tested and did not help drops to low.

## Exit codes

| Code | When |
|---|---|
| 0 | The report was printed (or the viewer closed normally). |
| 1 | Something went wrong (no plan in the input, a connection error, a refused statement), or `--fail-on` was given and a finding is at least that severe. |

`explainsql check` has its own: 0 when every plan passed, 1 when one failed, 2 when the check could not run. A reader that closes the pipe early, such as `head`, is not an error.

## Checking how a plan was read

If a report looks wrong, the first question is whether the plan was read correctly. `--debug-parse` prints what the parser understood instead of the analysis: the format it detected, the tree with estimates and actuals, the statement's summary and any warnings. With `--format json`, it prints the parsed plan as JSON.

```text
$ explainsql --demo --debug-parse
format: Text
Nested Loop  [estimated rows 15 cost 91527.00]  [actual rows 14 × 1 loops, 369.223 ms per loop]
  Seq Scan on public.orders o  [estimated rows 10 cost 4917.00]  [actual rows 10 × 1 loops, 12.134 ms per loop]
  Seq Scan on public.order_items oi  [estimated rows 300000 cost 4911.00]  [actual rows 300000 × 10 loops, 18.926 ms per loop]
statement: planning 0.526 ms; execution 369.281 ms; settings: enable_hashjoin=off, enable_material=off, enable_mergejoin=off, max_parallel_workers_per_gather=0
warnings: none
```

ExplainSQL never gives up on a plan it partly understands: unfamiliar properties are kept, unreadable lines are set aside with a warning, and a truncated plan keeps the nodes before the cut. If you find a plan it reads wrongly, please [open an issue](https://github.com/onplt/explain-sql/issues) with it, anonymized with [`explainsql anonymize`](anonymize.md) if needed.
