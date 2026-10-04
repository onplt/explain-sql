-- description: Workers sort in parallel and a Gather Merge combines the sorted streams.
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT id, amount FROM orders WHERE amount > 900 ORDER BY amount;
