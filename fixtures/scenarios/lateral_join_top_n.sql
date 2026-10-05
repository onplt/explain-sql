-- description: LATERAL top-3 per customer; without an index on customer_id, every loop walks the created_at index backwards and discards most of the rows it reads.
-- rules: ES005, ES006
-- advice: index
-- index: orders (customer_id, created_at)
-- set: max_parallel_workers_per_gather = 0
SELECT c.id, o.id, o.created_at
FROM customers c
CROSS JOIN LATERAL (
    SELECT id, created_at
    FROM orders
    WHERE orders.customer_id = c.id
    ORDER BY created_at DESC
    LIMIT 3
) o
WHERE c.id <= 20;
