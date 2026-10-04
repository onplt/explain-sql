-- description: Aggregate over a whole table; a sequential scan is the right plan.
-- advice: none
-- set: max_parallel_workers_per_gather = 0
SELECT count(*), sum(amount) FROM orders;
