-- description: CTE referenced twice, so it is materialized once and read by two CTE Scan nodes.
-- set: max_parallel_workers_per_gather = 0
WITH per_status AS (
    SELECT status, count(*) AS n FROM orders GROUP BY status
)
SELECT a.status, b.status FROM per_status a JOIN per_status b ON a.n < b.n;
