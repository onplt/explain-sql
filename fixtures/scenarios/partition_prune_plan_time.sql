-- description: Only one partition is scanned because the constant range is pruned at plan time.
-- set: max_parallel_workers_per_gather = 0
SELECT count(*) FROM events
WHERE created_at >= timestamptz '2025-03-01 00:00:00+00'
  AND created_at < timestamptz '2025-04-01 00:00:00+00';
