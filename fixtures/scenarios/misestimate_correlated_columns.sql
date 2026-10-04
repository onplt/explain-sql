-- description: city determines country, but the planner multiplies their selectivities and underestimates the row count 20x.
-- rules: ES002
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM addresses WHERE city = 42 AND country = 0;
