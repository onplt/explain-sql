-- description: Hash join of every order with its customer; whole tables are needed, so no index helps.
-- advice: none
-- set: max_parallel_workers_per_gather = 0
SELECT c.country, count(*)
FROM orders o
JOIN customers c ON c.id = o.customer_id
GROUP BY c.country;
