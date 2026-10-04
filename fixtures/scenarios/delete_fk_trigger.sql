-- description: DELETE on a referenced table; the foreign-key trigger scans the unindexed order_items.order_id for every deleted row.
-- rules: ES009
-- advice: index
DELETE FROM orders WHERE id > 199980;
