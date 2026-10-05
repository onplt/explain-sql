-- description: jsonb containment filter on every partition of a partitioned table; only a GIN index could help.
-- advice: index
-- index: events USING gin (payload)
-- set: max_parallel_workers_per_gather = 0
SELECT id, created_at FROM events WHERE payload @> '{"n": 7}';
