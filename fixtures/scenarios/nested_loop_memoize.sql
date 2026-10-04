-- description: Nested loop with a Memoize cache in front of the inner index scan (PostgreSQL 14 and later).
-- min_version: 14
-- set: enable_hashjoin = off
-- set: enable_mergejoin = off
-- set: max_parallel_workers_per_gather = 0
SELECT oi.id, p.name
FROM order_items oi
JOIN products p ON p.id = oi.product_id
WHERE oi.id <= 20000;
