-- description: Comparing with a date makes the predicate stable rather than immutable, so partitions are removed at executor startup.
-- set: max_parallel_workers_per_gather = 0
SELECT count(*) FROM events WHERE created_at >= date '2025-12-01';
