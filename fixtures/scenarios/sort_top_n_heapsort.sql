-- description: ORDER BY ... LIMIT on an unindexed column: a top-N heapsort over the whole table.
-- advice: index
-- set: max_parallel_workers_per_gather = 0
SELECT id, amount FROM orders ORDER BY amount DESC LIMIT 20;
