-- description: Filter on a 50-row table, where an index would not help.
-- advice: none
SELECT * FROM settings_kv WHERE value = 'value3';
