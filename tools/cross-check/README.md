# Cross-checking the metrics engine

ExplainSQL's exclusive times are compared, node by node, with those of two established tools:

- **explain.depesz.com**, whose engine is the Perl module [Pg::Explain](https://metacpan.org/dist/Pg-Explain);
- **[pev2](https://github.com/dalibo/pev2)** (explain.dalibo.com), whose plan service runs headless in Node.

`compare.py` runs both sides on 24 reference plans from `fixtures/pg/{13,16,18}` (the text forms). It matches nodes by their actual time, rows and loops. A node agrees when the two exclusive times are within 5% of each other, or within 0.01 ms.

## Running it

Build the binary first (`cargo build -p explainsql`), then run the comparisons from the repository root.

**depesz.** Pg::Explain needs Clone, JSON::MaybeXS, XML::Simple, XML::LibXML, YAML::XS, HOP::Lexer and HOP::Stream. Install them from your distribution or with `cpanm`, then run:

```sh
cpanm Pg::Explain
python3 tools/cross-check/compare.py depesz perl tools/cross-check/depesz.pl
```

**pev2.**

```sh
(cd tools/cross-check && npm install)
python3 tools/cross-check/compare.py pev2 node tools/cross-check/pev2.mjs tools/cross-check/node_modules/pev2/dist/pev2.es.js
```

## Results

Pg::Explain 2.10 and pev2 1.24.0, October 2026:

| Corpus | depesz | pev2 |
|---|---|---|
| PostgreSQL 13 | 95/95 nodes | 95/95 nodes |
| PostgreSQL 16 | 97/99 nodes | 97/99 nodes |
| PostgreSQL 18 | 98/99 nodes | 98/99 nodes |

The only disagreement is deliberate: the Memoize plan (`nested_loop_memoize`) on PostgreSQL 16 and 18. There, a cache hit prints as `0.000` or `0.001 ms` per loop over 20,000 loops, so the printed times are off by up to 10 ms.
- **Both other tools:** they clamp the resulting negative exclusive time to zero. The exclusive times then add up to about 5 ms more than the statement took.
- **ExplainSQL:** it moves the rounding error to the figures that carry it, those with many loops. The exclusive times then add up to the measured total.

See "Metrics" in [docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md).
