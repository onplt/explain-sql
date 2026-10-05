-- description: DELETE on a referenced table; the foreign-key trigger scans the unindexed order_items.order_id for every deleted row.
-- rules: ES009
-- advice: index
-- index: constraint order_items_order_id_fkey
DELETE FROM orders WHERE id > 199980;
