-- description: enable_seqscan = off on a table without indexes; PostgreSQL 18 marks the node as disabled, older versions add disable_cost to its estimate.
-- set: enable_seqscan = off
SELECT * FROM audit_log WHERE action = 'login';
