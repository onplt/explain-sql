-- An application's requests, as an ORM runs them: see README.md.
-- Run with psql 16 or later, which sends \bind statements with the
-- extended query protocol, as drivers do.

-- Request 1: the latest orders of customer 4242, each with its items, then their product.
SELECT id, status, created_at FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90200-01'*/ \bind 4242 5 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90201-01'*/ \bind 76639 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90202-01'*/ \bind 176639 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90203-01'*/ \bind 16639 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90204-01'*/ \bind 116639 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90205-01'*/ \bind 56639 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90210-01'*/ \bind 1308 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90211-01'*/ \bind 1308 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba90212-01'*/ \bind 1308 \g
\! sleep 0.3

-- Request 2: the latest orders of customer 777, each with its items, then their product.
SELECT id, status, created_at FROM orders WHERE customer_id = $1 ORDER BY created_at DESC LIMIT $2 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90300-01'*/ \bind 777 5 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90301-01'*/ \bind 18904 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90302-01'*/ \bind 118904 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90303-01'*/ \bind 58904 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90304-01'*/ \bind 158904 \g
SELECT id, product_id, quantity FROM order_items WHERE order_id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90305-01'*/ \bind 98904 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90310-01'*/ \bind 753 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90311-01'*/ \bind 753 \g
SELECT id, name, price FROM products WHERE id = $1 /*action='latest',controller='OrderController',framework='spring',traceparent='00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba90312-01'*/ \bind 753 \g
\! sleep 0.3

-- Request 3: no tags, in a transaction, values written into the text.
BEGIN;
SELECT id, name FROM customers WHERE country = 'TR' ORDER BY id LIMIT 3;
SELECT count(*) FROM orders WHERE customer_id = 10;
SELECT count(*) FROM orders WHERE customer_id = 20;
SELECT count(*) FROM orders WHERE customer_id = 30;
COMMIT;
\! sleep 0.3

-- Request 4: no tags, no transaction: the same setting read four times.
SELECT value FROM settings_kv WHERE key = 'key7';
SELECT value FROM settings_kv WHERE key = 'key7';
SELECT value FROM settings_kv WHERE key = 'key7';
SELECT value FROM settings_kv WHERE key = 'key7';
SELECT id, email FROM customers WHERE id = 4242;
