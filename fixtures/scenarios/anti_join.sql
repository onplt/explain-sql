-- description: NOT EXISTS subquery, planned as an anti join.
-- rules: ES001
-- set: max_parallel_workers_per_gather = 0
SELECT c.id
FROM customers c
WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.status = 'refunded');
