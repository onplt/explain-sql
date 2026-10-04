-- description: Statistics predate 50,000 inserted 'active' rows, so the estimate is off by orders of magnitude.
-- rules: ES002
-- advice: none
-- set: max_parallel_workers_per_gather = 0
SELECT count(*) FROM shipments WHERE state = 'active';
