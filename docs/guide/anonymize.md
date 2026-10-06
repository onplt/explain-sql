# Share a plan

A plan says a lot about your database: the names of its tables, columns and indexes, and the values your statement looked for, which may be customer emails or order numbers. Before a plan goes into a bug report, an issue or a chat, `explainsql anonymize` replaces all of that, while keeping everything needed to analyze it.

```sh
explainsql anonymize plan.json > shared.json
pbpaste | explainsql anonymize | pbcopy
explainsql anonymize plan.txt --map names.json   # and keep a note of what each name became
```

## What changes

- **Names** of tables, indexes, CTEs, aliases, schemas, columns, constraints and triggers become `table_a`, `index_a`, `cte_a`, `alias_a`, `schema_a`, `column_a`, `constraint_a` and `trigger_a`, then `_b`, `_c` and so on.
- **Names that differ only in their numbers**, as partitions do, stay alike: `orders_2025_01` and `orders_2025_02` become `table_b_1` and `table_b_2`, so the viewer still folds them and `explainsql diff` still matches them.
- **String literals** become `'value_a'`, `'value_b'` and so on, keeping a `LIKE` pattern's `%` at either end. **Numbers** in conditions become other numbers of the same form.
- **The statement's text** (`Query Text`) is anonymized the same way, and its comments are dropped.

The same name or value gets the same replacement everywhere, in every plan of the input, so the anonymized plan stays consistent.

## What stays

Node types, estimates, timings, buffers and every other figure, so the plan reads and analyzes exactly as before: it gets the same findings, and compares with another plan as the original does. Function and type names, keywords, `$n` parameters and system names (`pg_catalog`, `public`, `pg_…` relations, `ctid`, the triggers behind foreign keys) are kept too.

## Input and output

It reads anything ExplainSQL reads, with every plan in it. The plans come out in the format they went in, JSON or text, without whatever surrounded them (psql's table, log lines, a Markdown fence). When an input holds plans in both formats, each comes out in its own Markdown fence.

To stay on the safe side, a line or JSON property it does not recognize has every name and value in it replaced. And if the anonymized plans do not read back with the same nodes, nothing is printed at all.

## Options

- `--keep-names` replaces only the literal values and keeps the names, for when the schema is not secret but the data is.
- `--map FILE` writes what each name and value became, as JSON, so that you can translate an answer about the anonymized plan back. That file holds the originals, so keep it to yourself.
