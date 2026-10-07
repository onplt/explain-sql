-- Delete orders that have no items. Each deleted row fires a foreign-key
-- check on order_items, whose order_id has no index. Rolled back.
-- Try: --allow-dml, W, and the advice for the foreign key
DELETE FROM orders
WHERE id > 199950;
