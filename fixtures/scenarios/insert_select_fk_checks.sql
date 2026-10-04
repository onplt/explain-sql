-- description: INSERT ... SELECT that fires a foreign-key check for every inserted row.
INSERT INTO order_items (id, order_id, product_id, quantity, unit_price)
SELECT 1000000 + i, 1 + i, 1 + i % 5000, 1, 9.99
FROM generate_series(1, 1000) AS i;
