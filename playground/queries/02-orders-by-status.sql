-- Orders by status, sent with a parameter as an application would.
-- 'delivered' is 70% of the rows, 'refunded' 1%.
-- Try: --params. Expect INSENSITIVE: every value gets the same plan, so a
-- parameter alone is not always a problem. Compare with 03-latest-orders.sql.
SELECT *
FROM orders
WHERE status = $1
ORDER BY created_at DESC
LIMIT 20;
