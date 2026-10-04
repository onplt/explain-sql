-- description: The partition key is compared with an InitPlan result, so pruning happens during execution and pruned partitions are never executed.
-- set: max_parallel_workers_per_gather = 0
SELECT count(*) FROM events
WHERE created_at >= (SELECT max(created_at) - interval '7 days' FROM events_2025_06);
