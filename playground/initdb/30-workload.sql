-- A small application workload, so that `explainsql top` has something to
-- list. Every statement runs on its own, with different constants, the way
-- an application sends them; pg_stat_statements folds the constants into
-- $1, $2, …
--
-- Loading schema.sql is not part of it: the statistics are reset first.
SELECT pg_stat_statements_reset();

-- A customer's order summary: a sequential scan of orders, every time.
SELECT format($$SELECT c.name, count(*) AS orders, sum(o.amount) AS total
FROM customers c JOIN orders o ON o.customer_id = c.id
WHERE c.id = %s GROUP BY c.name$$, 1 + i * 397 % 20000)
FROM generate_series(1, 20) AS i \gexec

-- A customer's latest orders.
SELECT format($$SELECT * FROM orders WHERE customer_id = %s ORDER BY created_at DESC LIMIT 10$$,
              1 + i * 811 % 20000)
FROM generate_series(1, 15) AS i \gexec

-- Orders by status: 'delivered' is 70% of the rows, 'refunded' 1%.
SELECT format($$SELECT * FROM orders WHERE status = %L ORDER BY created_at DESC LIMIT 20$$, s)
FROM unnest(ARRAY['delivered', 'shipped', 'pending', 'cancelled', 'refunded']) AS s,
     generate_series(1, 4) \gexec

-- The items of one order: order_items.order_id has no index.
SELECT format($$SELECT * FROM order_items WHERE order_id = %s$$, 1 + i * 7907 % 190000)
FROM generate_series(1, 10) AS i \gexec

-- A day of orders, with the date wrapped in a function.
SELECT format($$SELECT count(*) FROM orders WHERE date_trunc('day', created_at) = %L::timestamptz$$,
              timestamptz '2024-06-01 00:00:00+00' + i * interval '1 day')
FROM generate_series(1, 8) AS i \gexec

-- Correlated columns: city determines country.
SELECT format($$SELECT * FROM addresses WHERE city = %s AND country = %s$$, c, c / 50)
FROM generate_series(40, 49) AS c \gexec

-- Stale statistics: ANALYZE never saw the 'active' shipments.
SELECT format($$SELECT count(*) FROM shipments WHERE state = %L$$, s)
FROM unnest(ARRAY['active', 'archived']) AS s, generate_series(1, 3) \gexec

-- Lookups that are fast, for contrast.
SELECT format($$SELECT * FROM customers WHERE id = %s$$, i)
FROM generate_series(1, 50) AS i \gexec
SELECT format($$SELECT value FROM settings_kv WHERE key = %L$$, 'key' || i)
FROM generate_series(1, 50) AS i \gexec

-- Leave out the statements above that wrote the workload.
SELECT count(pg_stat_statements_reset(0, 0, queryid))
FROM pg_stat_statements
WHERE query LIKE 'SELECT format(%' OR query LIKE 'SELECT pg_stat_statements_reset%';
