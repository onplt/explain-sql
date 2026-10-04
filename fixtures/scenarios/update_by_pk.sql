-- description: UPDATE of a primary key range (ModifyTable over an index scan).
UPDATE orders SET note = upper(note) WHERE id BETWEEN 1000 AND 1100;
