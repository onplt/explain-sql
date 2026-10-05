-- description: Sequential scan whose filter keeps 10 of 200,000 rows because customer_id has no index.
-- rules: ES001
-- advice: index
-- index: orders (customer_id)
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id = 4242;
