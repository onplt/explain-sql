-- A customer's latest orders, with JDBC-style placeholders.
-- Try: --params --measure (the generic plan does much worse for big LIMITs)
SELECT *
FROM orders
WHERE customer_id = ?
ORDER BY created_at DESC
LIMIT ?;
