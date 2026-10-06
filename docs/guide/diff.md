# Compare two plans

A plan changed after you added an index, refreshed statistics, upgraded PostgreSQL or rewrote the query. Reading two big plans side by side to see what moved is tedious and error-prone. `explainsql diff` does it for you, node by node.

```sh
explainsql diff before.json after.json
explainsql diff plans.txt                     # both plans in one input
explainsql diff before.json after.txt --format md
```

The two plans can come in any form ExplainSQL reads, and they do not have to match: JSON against text is fine. A single input can also hold both, one after the other: two plans pasted one below the other (a label such as `After:` between them is ignored), a JSON array or two JSON documents, two Markdown code fences, two psql results, or two auto_explain entries from a log.

## The report

It opens with one sentence: how the second plan compares, pages first, then time (or the estimated cost when the plans were not run), and its main change. Then come the changes, the most significant first:

| Change | What it means |
|---|---|
| `ACCESS` | A relation is read another way: another scan type, index or direction, or in parallel. The same change on several partitions is reported once. |
| `JOIN` | The same relations are joined with another method, or the sides of the join swapped. |
| `ORDER` | The relations are joined in another order. |
| `STRATEGY` | Another variant of the same operation: a hashed aggregate that became sorted, a sort that became incremental. |
| `ADDED`, `REMOVED` | A node only one plan has, such as a Sort that an index made unnecessary, or a Gather that runs part of the plan in parallel. Partitions read or no longer read are counted together. |
| `SPILL` | A node started or stopped writing temporary files. |
| `ESTIMATE` | A row estimate became 10× off or more, or stopped being, at the node where the error starts. |
| `WORK` | The same node read more or fewer pages, or took more or less time, by more than 10% and 5% of the statement. A change in time alone, for the same pages, says how many of them came from disk, since the cache or the server's load may explain it rather than the plan. |

Last comes the plan after, with changed nodes marked `~` and new ones `+`, followed by the nodes only the plan before had.

Text is the default; `--format md` is ready for a pull request or an issue, and `--format json` holds the full diff with every node of both plans.

## How nodes are matched

Nodes are matched by the work they do, not by their position in the tree. A scan is found again by the relation it reads, a join by the relations it combines, and any other node by its kind and the relations below it. Partitions that PostgreSQL named differently, as different versions do, still match. So do plans that grew or lost a node in the middle.

## Plan shapes

Every plan has a shape: 16 hexadecimal digits that stand for its nodes, what they read and how, leaving out numbers, literal values and aliases. Two plans with the same shape are the same plan, whatever the parameters, the data or the cache, and whether they were printed as JSON or text. The diff shows both shapes, and [`explainsql check`](ci.md) and [`explainsql logs`](logs.md) use them to notice when a plan changes.

## In the viewer

In [connected mode](connected.md), after `r` or `e` runs the statement again, the status line compares the new run with the previous one the same way.
