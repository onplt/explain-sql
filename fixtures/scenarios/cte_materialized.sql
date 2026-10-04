-- description: MATERIALIZED CTE scanned once: a CTE Scan plus the CTE subplan.
-- set: max_parallel_workers_per_gather = 0
WITH totals AS MATERIALIZED (
    SELECT customer_id, sum(amount) AS total FROM orders GROUP BY customer_id
)
SELECT c.name, t.total
FROM totals t
JOIN customers c ON c.id = t.customer_id
WHERE t.total > 5900;
