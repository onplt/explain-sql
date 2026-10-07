-- A year and a half of orders. orders.created_at has an index, yet the planner
-- reads the whole table.
-- Try: --why-not (or y on the scan): the answer is why the index loses.
SELECT id, customer_id, amount
FROM orders
WHERE created_at >= timestamptz '2024-03-01 00:00:00+00'
  AND created_at <  timestamptz '2025-09-01 00:00:00+00';
