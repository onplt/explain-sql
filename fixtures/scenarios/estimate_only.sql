-- description: Plain EXPLAIN without ANALYZE: estimates only.
-- options: VERBOSE, SETTINGS
SELECT o.id, c.name
FROM orders o
JOIN customers c ON c.id = o.customer_id
WHERE o.status = 'refunded';
