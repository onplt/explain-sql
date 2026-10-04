-- description: Recursive CTE: a Recursive Union over a WorkTable Scan.
WITH RECURSIVE series (n) AS (
    SELECT 1
    UNION ALL
    SELECT n + 1 FROM series WHERE n < 1000
)
SELECT count(*) FROM series;
