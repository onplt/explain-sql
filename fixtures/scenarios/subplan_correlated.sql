-- description: Correlated scalar subquery run once per outer row (SubPlan), each time scanning all of order_items.
-- advice: index
-- index: order_items (order_id)
-- set: max_parallel_workers_per_gather = 0
SELECT o.id, (SELECT count(*) FROM order_items oi WHERE oi.order_id = o.id) AS items
FROM orders o
WHERE o.customer_id = 4242;
