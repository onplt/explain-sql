-- description: Parallel sequential scan under a Gather node.
-- rules: ES001, ES002?
-- advice: index
-- index: orders (note text_pattern_ops)
-- set: parallel_setup_cost = 0
-- set: parallel_tuple_cost = 0
-- set: min_parallel_table_scan_size = 0
SELECT id, note FROM orders WHERE note LIKE 'ab%';
