-- description: EXPLAIN (GENERIC_PLAN) of a parameterized query as it appears in application logs (PostgreSQL 16 and later).
-- min_version: 16
-- options: GENERIC_PLAN, VERBOSE, SETTINGS
SELECT * FROM orders WHERE customer_id = $1 AND status = $2;
