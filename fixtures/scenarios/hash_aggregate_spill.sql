-- description: Hash aggregate that spills to disk because work_mem is tiny (PostgreSQL 13 and later).
-- rules: ES004, ES013
-- min_version: 13
-- set: work_mem = '64kB'
-- set: enable_sort = off
-- set: max_parallel_workers_per_gather = 0
SELECT customer_id, count(*), sum(amount) FROM orders GROUP BY customer_id;
