-- description: The outer side returns no rows, so the inner side of the nested loop is never executed.
-- rules: ES013
-- set: enable_hashjoin = off
-- set: enable_mergejoin = off
-- set: max_parallel_workers_per_gather = 0
SELECT o.id FROM customers c JOIN orders o ON o.customer_id = c.id WHERE c.country = 'XX';
