-- Mark a customer's orders as shipped. Runs only with --allow-dml, and is
-- always rolled back.
-- Try: W (what the writes cost), L (locks), --allow-ddl --prove
UPDATE orders
SET status = 'shipped', created_at = now()
WHERE customer_id = 4242;
