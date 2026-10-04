-- description: JIT compilation forced on a tiny query, so compiling takes far longer than executing.
-- rules: ES012
-- requires: jit
-- set: jit = on
-- set: jit_above_cost = 0
-- set: jit_inline_above_cost = 0
-- set: jit_optimize_above_cost = 0
-- set: max_parallel_workers_per_gather = 0
SELECT count(*), sum(amount) FROM orders WHERE id <= 100;
