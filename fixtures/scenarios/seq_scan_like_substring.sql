-- description: Substring LIKE search; only a pg_trgm GIN index could help.
-- rules: ES001
-- advice: index
-- index: orders USING gin (note gin_trgm_ops)
-- set: max_parallel_workers_per_gather = 0
SELECT id, note FROM orders WHERE note LIKE '%abcd%';
