-- description: Bitmap heap scan over most of the table with a tiny work_mem, so the bitmap becomes lossy and rows are rechecked.
-- rules: ES008
-- set: work_mem = '64kB'
-- set: enable_seqscan = off
-- set: enable_indexscan = off
-- set: max_parallel_workers_per_gather = 0
SELECT sum(amount) FROM orders
WHERE created_at >= timestamptz '2024-01-01 00:00:00+00'
  AND created_at < timestamptz '2025-07-01 00:00:00+00';
