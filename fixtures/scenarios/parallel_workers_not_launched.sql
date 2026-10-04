-- description: The plan asks for two workers but none can start because max_parallel_workers is 0.
-- rules: ES011
-- set: max_parallel_workers = 0
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT count(*) FROM orders WHERE amount > 500;
