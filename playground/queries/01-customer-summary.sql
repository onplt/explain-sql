-- A customer's order summary. orders.customer_id has no index, so the
-- whole orders table is scanned to find ten rows.
-- Try: the verdict, 1 (slowest node), i (advice), t (test the index),
-- y (ask the planner why), F (icicle), L (locks).
SELECT c.name, count(*) AS orders, sum(o.amount) AS total
FROM customers c
JOIN orders o ON o.customer_id = c.id
WHERE c.id = 4242
GROUP BY c.name;
