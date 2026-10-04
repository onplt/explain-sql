-- description: GROUP BY on a low-cardinality column (hash aggregate).
-- set: max_parallel_workers_per_gather = 0
SELECT status, count(*), sum(amount) FROM orders GROUP BY status;
