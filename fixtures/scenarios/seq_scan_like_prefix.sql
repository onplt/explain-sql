-- description: LIKE prefix search under a non-C collation; a btree index would need text_pattern_ops.
-- rules: ES001
-- advice: index
-- set: max_parallel_workers_per_gather = 0
SELECT id, note FROM orders WHERE note LIKE 'abc%';
