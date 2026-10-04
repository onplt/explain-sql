-- description: INSERT with the WAL option: WAL records, full-page images and bytes (PostgreSQL 13 and later).
-- min_version: 13
-- options: ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS
INSERT INTO audit_log (id, action, at)
SELECT 100000 + i, 'import', timestamptz '2025-06-01 00:00:00+00'
FROM generate_series(1, 5000) AS i;
