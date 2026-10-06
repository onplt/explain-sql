-- A customer's order summary
SELECT c.name, count(*) AS orders, sum(o.amount) AS total
FROM customers c
JOIN orders o ON o.customer_id = c.id
WHERE c.id = 4242
GROUP BY c.name;
