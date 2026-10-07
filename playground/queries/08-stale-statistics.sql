-- ANALYZE never saw the 50,000 'active' shipments, so the estimate is off
-- by orders of magnitude.
SELECT count(*)
FROM shipments
WHERE state = 'active';
