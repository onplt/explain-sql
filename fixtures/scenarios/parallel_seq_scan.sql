-- description: Parallel sequential scan under a Gather node.
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT id, note FROM orders WHERE note LIKE 'ab%';
