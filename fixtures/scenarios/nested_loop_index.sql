-- description: Nested loop with a primary key lookup on the inner side (the good case).
SELECT o.id, c.name
FROM orders o
JOIN customers c ON c.id = o.customer_id
WHERE o.created_at >= timestamptz '2024-03-01 10:00:00+00'
  AND o.created_at < timestamptz '2024-03-01 11:00:00+00';
