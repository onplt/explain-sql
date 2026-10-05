-- description: Nested loop that scans all of order_items for every outer row because order_id has no index.
-- rules: ES001, ES005
-- advice: index
-- index: order_items (order_id)
-- set: enable_hashjoin = off
-- set: enable_mergejoin = off
-- set: enable_material = off
-- set: max_parallel_workers_per_gather = 0
SELECT o.id, oi.product_id
FROM orders o
JOIN order_items oi ON oi.order_id = o.id
WHERE o.customer_id = 4242;
