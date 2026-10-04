-- description: No partition key in the predicate, so an Append scans all twelve partitions.
-- set: max_parallel_workers_per_gather = 0
SELECT kind, count(*) FROM events GROUP BY kind;
