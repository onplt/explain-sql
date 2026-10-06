-- description: Hash join whose hash table does not fit in work_mem and is split into batches on disk.
-- rules: ES004, ES013
-- set: work_mem = '64kB'
-- set: enable_mergejoin = off
-- set: max_parallel_workers_per_gather = 0
SELECT count(*) FROM orders o JOIN order_items oi ON oi.order_id = o.id;
