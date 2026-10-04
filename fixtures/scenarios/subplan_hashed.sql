-- description: NOT IN over a subquery, evaluated as a hashed SubPlan.
-- set: max_parallel_workers_per_gather = 0
SELECT id FROM customers WHERE id NOT IN (SELECT customer_id FROM orders WHERE status = 'refunded');
