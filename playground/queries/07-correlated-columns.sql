-- city determines country, but the planner multiplies their selectivities
-- and expects 20 times fewer rows than it gets (▼ in the tree).
SELECT *
FROM addresses
WHERE city = 42 AND country = 0;
