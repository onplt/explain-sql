# As psql's pager

An EXPLAIN tool is something you reach for now and then, which makes it easy to forget. The pager mode fixes that: you keep working in psql exactly as before, and every plan simply opens in the viewer.

## Set it up

Add this to your shell's profile:

```sh
export PSQL_PAGER='explainsql --pager'
```

and this to your `~/.psqlrc`, so that short plans, which fit on the screen, go to the pager too:

```text
\pset pager always
```

Now run an `EXPLAIN` in psql:

```sql
EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM orders WHERE customer_id = 42;
```

The plan opens in the viewer. Press `q` and you are back at the psql prompt.

## What happens to everything else

ExplainSQL looks at what psql sends to the pager. A plan opens in the viewer. Anything else, such as the result of a `SELECT`, goes on to your usual pager: `$EXPLAINSQL_PAGER` if set, otherwise `$PAGER`, otherwise `less -S`. It never hands output back to `explainsql`, so setting `PAGER` to ExplainSQL itself cannot loop. If none of those pagers can run, the output is printed directly.

When the output is not a terminal, for instance when you redirect psql's output to a file, everything passes through unchanged.

So if you like `pspg` for result sets, keep it:

```sh
export PSQL_PAGER='explainsql --pager'
export EXPLAINSQL_PAGER='pspg'
```

## Tips

- Use `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` for the richest analysis, and turn on `track_io_timing` in the session if you can. [Getting started](../getting-started.md#capture-plans-that-say-more) explains what each option adds.
- Both the text and JSON formats work, as does psql's expanded mode (`\x`).
- Without the pager, you can still send a single result to ExplainSQL with psql's `\g | explainsql`.
- The pager mode reads plans only. To test an index or ask the planner why, run the statement in [connected mode](connected.md).
