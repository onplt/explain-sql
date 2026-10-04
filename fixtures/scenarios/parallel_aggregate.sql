-- description: Partial aggregates in parallel workers, combined by a Finalize Aggregate.
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT count(*), avg(amount) FROM orders;
