-- description: Missing join condition: every selected customer is paired with every book.
-- rules: ES010
-- set: max_parallel_workers_per_gather = 0
SELECT c.id, p.id FROM customers c, products p WHERE c.id <= 100 AND p.category = 'books';
