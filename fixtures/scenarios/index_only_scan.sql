-- description: Index-only scan on a freshly vacuumed table, with no heap fetches.
SELECT created_at FROM orders
WHERE created_at >= timestamptz '2024-03-01 00:00:00+00'
  AND created_at < timestamptz '2024-03-02 00:00:00+00';
