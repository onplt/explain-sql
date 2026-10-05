-- description: Index range scan on created_at that discards most rows with a filter on status.
-- rules: ES006
-- advice: index
-- index: orders (status, created_at)
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders
WHERE created_at >= timestamptz '2024-06-01 00:00:00+00'
  AND created_at < timestamptz '2024-06-15 00:00:00+00'
  AND status = 'refunded';
