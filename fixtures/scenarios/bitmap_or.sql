-- description: BitmapOr that combines two ranges on the same index.
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders
WHERE created_at < timestamptz '2024-01-02 00:00:00+00'
   OR created_at >= timestamptz '2025-12-30 00:00:00+00';
