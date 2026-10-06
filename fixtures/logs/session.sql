-- The session behind the logs in this directory. Run it twice against the
-- fixture schema on a server that logs to stderr, csvlog and jsonlog, with
-- compute_query_id = on: psql -v format=text -f session.sql, then
-- psql -v format=json -f session.sql. See README.md.
--
-- An application's statements, logged by auto_explain: a customer's latest
-- orders, served by an index until a migration drops it; a daily report
-- whose plan never changes; and a prepared statement that switches to its
-- generic plan after five executions. The queries carry sqlcommenter tags.
\set ON_ERROR_STOP on
LOAD 'auto_explain';
SET auto_explain.log_min_duration = 0;
SET auto_explain.log_analyze = on;
SET auto_explain.log_buffers = on;
SET auto_explain.log_verbose = on;
SET auto_explain.log_settings = on;
SET auto_explain.log_format = :'format';

CREATE INDEX orders_customer_id_idx ON orders (customer_id);
ANALYZE orders;

SET application_name = 'shop-api';
SELECT id, status, amount FROM orders WHERE customer_id = 4242 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'*/;
SELECT id, status, amount FROM orders WHERE customer_id = 17 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'*/;
SELECT id, status, amount FROM orders WHERE customer_id = 9001 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-5d41402abc4b2a76b9719d911017c592-7d793037a0760186-01'*/;

SET application_name = 'shop-reports';
SELECT status, count(*) FROM orders WHERE created_at >= timestamptz '2025-06-01 00:00:00+00' AND created_at < timestamptz '2025-06-02 00:00:00+00' GROUP BY status /*action='daily',controller='ReportController',framework='spring'*/;

-- A migration drops the index.
DROP INDEX orders_customer_id_idx;

SET application_name = 'shop-api';
SELECT id, status, amount FROM orders WHERE customer_id = 4242 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-7f2d1a3c9b8e4f6a0b1c2d3e4f5a6b7c-1a2b3c4d5e6f7081-01'*/;
SELECT id, status, amount FROM orders WHERE customer_id = 17 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-9e107d9d372bb6826bd81d3542a419d6-0f1e2d3c4b5a6978-01'*/;
SELECT id, status, amount FROM orders WHERE customer_id = 9001 ORDER BY created_at DESC LIMIT 10 /*action='latest',controller='OrderController',framework='spring',traceparent='00-e4d909c290d0fb1ca068ffaddf22cbd0-8899aabbccddeeff-01'*/;

SET application_name = 'shop-reports';
SELECT status, count(*) FROM orders WHERE created_at >= timestamptz '2025-06-02 00:00:00+00' AND created_at < timestamptz '2025-06-03 00:00:00+00' GROUP BY status /*action='daily',controller='ReportController',framework='spring'*/;

-- A prepared statement, as a driver prepares it: five custom plans, then
-- the generic plan.
SET application_name = 'shop-batch';
PREPARE latest (integer, bigint) AS
  SELECT id, status, amount FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2;
EXECUTE latest(4242, 10);
EXECUTE latest(17, 10);
EXECUTE latest(9001, 10);
EXECUTE latest(123, 10);
EXECUTE latest(5555, 10);
EXECUTE latest(777, 10);
EXECUTE latest(3141, 10);
EXECUTE latest(2718, 10);
DEALLOCATE latest;
