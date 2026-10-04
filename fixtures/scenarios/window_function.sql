-- description: Window function partitioned by country.
-- set: max_parallel_workers_per_gather = 0
SELECT id, country, row_number() OVER (PARTITION BY country ORDER BY created_at)
FROM customers;
