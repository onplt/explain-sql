-- description: Uncorrelated scalar subquery evaluated once as an InitPlan.
-- set: max_parallel_workers_per_gather = 0
SELECT id, amount FROM orders WHERE amount > (SELECT avg(amount) * 1.99 FROM orders);
