-- description: EXPLAIN ANALYZE with TIMING OFF: actual rows and loops, but no per-node times.
-- rules: ES001
-- advice: index
-- index: orders (customer_id)
-- options: ANALYZE, BUFFERS, TIMING OFF, VERBOSE, SETTINGS
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id = 4242;
