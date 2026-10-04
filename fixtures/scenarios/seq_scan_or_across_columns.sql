-- description: OR across two unindexed columns; no single index serves the predicate.
-- advice: none
-- set: max_parallel_workers_per_gather = 0
SELECT * FROM orders WHERE customer_id = 4242 OR status = 'refunded';
