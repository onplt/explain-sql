-- description: Sort that spills to disk because work_mem is tiny.
-- rules: ES003
-- set: work_mem = '64kB'
-- set: max_parallel_workers_per_gather = 0
SELECT id, note FROM orders ORDER BY note;
