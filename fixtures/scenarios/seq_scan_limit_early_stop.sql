-- description: LIMIT stops the sequential scan after a few hundred rows.
-- advice: none
SELECT * FROM orders WHERE status = 'pending' LIMIT 10;
