-- description: ORDER BY ... DESC LIMIT served by scanning an index backwards.
SELECT * FROM orders ORDER BY created_at DESC LIMIT 20;
