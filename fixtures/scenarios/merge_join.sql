-- description: Merge join, forced by disabling hash and nested-loop joins.
-- set: enable_hashjoin = off
-- set: enable_nestloop = off
-- set: max_parallel_workers_per_gather = 0
SELECT o.id, c.name
FROM orders o
JOIN customers c ON c.id = o.customer_id
WHERE o.created_at < timestamptz '2024-02-01 00:00:00+00';
