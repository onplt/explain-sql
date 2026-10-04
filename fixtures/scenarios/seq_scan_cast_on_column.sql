-- description: The column is cast to text, so even an index on customer_id could not be used.
-- rules: ES001, ES002
-- advice: rewrite
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id::text = '4242';
