-- description: Index-only scan on a table whose visibility map is out of date, so most rows need a heap fetch.
-- rules: ES007
-- set: enable_bitmapscan = off
SELECT page FROM page_views WHERE page BETWEEN 10 AND 19;
