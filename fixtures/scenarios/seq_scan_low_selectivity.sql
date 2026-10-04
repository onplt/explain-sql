-- description: Filter that keeps about 70% of the rows, so a sequential scan is the right plan.
-- advice: none
-- set: max_parallel_workers_per_gather = 0
SELECT id, amount FROM orders WHERE status = 'delivered';
