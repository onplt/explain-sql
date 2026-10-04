-- description: EXISTS subquery turned into a join: either a semi join or a join over de-duplicated subquery rows.
-- set: max_parallel_workers_per_gather = 0
SELECT p.id, p.name
FROM products p
WHERE EXISTS (SELECT 1 FROM order_items oi WHERE oi.product_id = p.id AND oi.quantity = 5);
