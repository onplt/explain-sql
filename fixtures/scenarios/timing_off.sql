-- description: EXPLAIN ANALYZE with TIMING OFF: actual rows and loops, but no per-node times.
-- rules: ES001
-- options: ANALYZE, BUFFERS, TIMING OFF, VERBOSE, SETTINGS
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id = 4242;
