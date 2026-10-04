-- description: Incremental sort on top of an index that provides the leading sort key (PostgreSQL 13 and later).
-- min_version: 13
SELECT * FROM orders ORDER BY created_at, id LIMIT 100;
