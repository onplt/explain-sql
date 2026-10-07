-- Extensions ExplainSQL uses in connected mode.
--
-- pg_stat_statements: `explainsql top`.
-- hypopg: `t` and `--prove` test a suggested index without building it.
--   DROP EXTENSION hypopg; to try the other path (`--allow-ddl`), and
--   CREATE EXTENSION hypopg; to get it back. The image installs it; on a
--   server without it, this script skips it.
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_available_extensions WHERE name = 'hypopg') THEN
        CREATE EXTENSION IF NOT EXISTS hypopg;
    ELSE
        RAISE WARNING 'HypoPG is not installed: `t` and --prove will need --allow-ddl';
    END IF;
END
$$;
