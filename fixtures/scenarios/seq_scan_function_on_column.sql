-- description: date_trunc() on a timestamptz column is not immutable, so the fix is a range predicate rather than an expression index.
-- rules: ES001
-- advice: rewrite
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE date_trunc('day', created_at) = timestamptz '2024-06-01 00:00:00+00';
