-- A day of orders, with the column wrapped in date_trunc(). The advice is a
-- rewrite of the condition, not an index.
SELECT count(*)
FROM orders
WHERE date_trunc('day', created_at) = timestamptz '2024-06-01 00:00:00+00';
