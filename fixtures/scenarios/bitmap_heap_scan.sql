-- description: Bitmap heap scan over a week of orders.
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders
WHERE created_at >= timestamptz '2024-03-01 00:00:00+00'
  AND created_at < timestamptz '2024-03-08 00:00:00+00';
