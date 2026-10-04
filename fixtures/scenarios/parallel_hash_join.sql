-- description: Parallel hash join between orders and order_items.
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT o.status, count(*)
FROM orders o
JOIN order_items oi ON oi.order_id = o.id
GROUP BY o.status;
