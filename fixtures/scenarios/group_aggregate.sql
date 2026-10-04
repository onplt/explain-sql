-- description: Sorted GROUP BY (group aggregate) with hash aggregation disabled.
-- set: enable_hashagg = off
-- set: max_parallel_workers_per_gather = 0
SELECT country, count(*) FROM customers GROUP BY country;
