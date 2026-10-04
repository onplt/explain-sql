-- description: EXPLAIN with SERIALIZE and MEMORY: output serialization cost and planner memory (PostgreSQL 17 and later).
-- min_version: 17
-- options: ANALYZE, BUFFERS, SERIALIZE, MEMORY, VERBOSE, SETTINGS
SELECT * FROM customers WHERE country = 'TR';
