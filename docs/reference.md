# Command-line reference

Everything ExplainSQL accepts, in one place. `explainsql --help` and `explainsql COMMAND --help` print the same information in your terminal.

```text
explainsql [OPTIONS] [FILE]       read a plan, or run a statement with -d
explainsql diff BEFORE [AFTER]    compare two plans
explainsql check PATHS…           check plans in CI
explainsql logs FILES…            plan changes in auto_explain logs
explainsql top -d DATABASE        the costliest statements from pg_stat_statements
explainsql requests FILES…        N+1 loops in statement logs
explainsql anonymize [FILE]       a plan with names and values replaced
```

## `explainsql`

Reads a plan from `FILE`, or from standard input when `FILE` is missing or `-`, and opens it in the [viewer](guide/viewer.md) or prints a [report](guide/reports.md). With `-d` and `-f` or `-c`, it runs a statement itself instead ([connected mode](guide/connected.md)).

### Reading and output

| Option | What it does |
|---|---|
| `FILE` | The plan file: JSON or text, as `EXPLAIN` printed it or wrapped in psql output, a log entry, a GUI client's cell or a Markdown fence. Standard input when missing or `-`. |
| `--print` | Print a report instead of opening the viewer. This happens anyway when the output is not a terminal. |
| `--format text\|md\|json` | The report's format. Default: `text`. |
| `--color auto\|always\|never` | When to color the text report. `auto` (the default) colors when the output is a terminal and `NO_COLOR` is not set. |
| `--theme dark\|light` | The terminal's background, for the viewer's colors. Default: `dark`. |
| `--demo` | Show the bundled sample plan instead of reading one. |
| `--pager` | Act as [psql's pager](guide/pager.md): open plans in the viewer, pass any other output to `$EXPLAINSQL_PAGER`, `$PAGER` or `less -S`. |
| `--debug-parse` | Print what the parser understood instead of the analysis. With `--format json`, the parsed plan as JSON. |
| `--fail-on low\|medium\|high` | With a printed report, exit with 1 when a finding is at least this severe. |

### Connected mode

| Option | What it does |
|---|---|
| `-d`, `--dbname DATABASE` | The database: a URL, `key=value` settings or a name. `PG*` variables, the service file and `~/.pgpass` apply as in psql. |
| `-f`, `--query-file FILE` | Run the statement in this file. |
| `-c`, `--command SQL` | Run this statement. |
| `--no-analyze` | Show the estimated plan only, without running the statement. |
| `--timeout SECONDS` | Stop a run after this many seconds. Default: 30. |
| `--allow-dml` | Also run statements that modify data or lock rows, still in a transaction that is rolled back, and report [what their writes cost](guide/writes.md). |
| `--allow-ddl` | To test an index without HypoPG, build it in a transaction that is rolled back. With `--prove`, also drop the indexes that keep updates from being HOT. Both block the table while they run. |
| `--prove` | With `--print`: [test each suggested index](guide/connected.md#test-a-suggested-index), and with `--allow-ddl`, rerun a non-HOT update without its blocking indexes. |
| `--why-not [TABLE]` | With `--print`: [ask the planner why](guide/why-not.md) it chose its plan for the slowest nodes, or for the scans of `TABLE` (a table or index name). |
| `--measure` | Measure the alternatives of `--why-not` and `y`, and the differing plans of `--params`, with `EXPLAIN ANALYZE` instead of only estimating them. |
| `--runs N` | How many measured runs to compare for `--prove` and `--measure`, each side after one warm-up run. The median counts. Default: 1. |
| `--locks` | Report [the locks the statement takes](guide/locks.md). |
| `--params` | The statement takes parameters (`$1`, or `?` as in JDBC): [compare the plans their values get](guide/parameters.md) with the generic plan. |
| `--bind N=VALUE` | The value of `$N`, tried instead of values from the statistics. Repeat for each parameter. Implies `--params`. |

## `explainsql diff`

```text
explainsql diff [OPTIONS] BEFORE [AFTER]
```

[Compares two plans](guide/diff.md) of the same statement, node by node. Without `AFTER`, `BEFORE` must hold both plans, one after the other.

| Option | What it does |
|---|---|
| `BEFORE` | The plan before: a file, or `-` for standard input. |
| `AFTER` | The plan after. |
| `--format text\|md\|json` | Default: `text`. |
| `--color auto\|always\|never` | Default: `auto`. |

## `explainsql check`

```text
explainsql check [OPTIONS] PATHS…
```

[Checks plans in CI](guide/ci.md): each against its findings and against the plan locked for it. Exits with 0 when every plan passed, 1 when one failed, 2 on an error.

| Option | What it does |
|---|---|
| `PATHS` | Plan files, or with `-d`, SQL files. Directories are searched for `*.json` and `*.txt` plans, or `*.sql` statements. |
| `-d`, `--dbname DATABASE` | Run the SQL files against this database. |
| `--fail-on low\|medium\|high` | Also fail a plan with a finding at least this severe. |
| `--strict` | Also fail a plan whose shape changed, even when it is not worse. |
| `--lock FILE` | The file of locked plans. Default: `explainsql.lock`. |
| `--update` | Lock the plans as they are now instead of checking them. Other plans in the file are kept. |
| `--prove` | With `-d`: test the suggested indexes of each plan that failed, with HypoPG. |
| `--no-analyze` | With `-d`: plan the statements without running them. |
| `--allow-dml` | With `-d`: also run statements that modify data or lock rows, rolled back. |
| `--timeout SECONDS` | With `-d`: stop a statement after this many seconds. Default: 30. |
| `--format text\|md\|json\|sarif` | Default: `text`. |
| `--sarif FILE` | Also write the SARIF report to this file. |
| `--color auto\|always\|never` | Default: `auto`. |

## `explainsql logs`

```text
explainsql logs [OPTIONS] FILES…
```

Reads [auto_explain plans from server logs](guide/logs.md) (stderr, csvlog or jsonlog; `-` for standard input) and reports when each statement's plan changed.

| Option | What it does |
|---|---|
| `--since TIME` | Only entries from this time on: as the log prints times (`2026-10-06 06:00`), or `30m`, `24h`, `7d` back from the last entry. |
| `--until TIME` | Only entries up to this time. |
| `--query ID\|TEXT` | Only the statement with this query identifier, or whose text, prepared name or tags contain this. |
| `--trace TRACE_ID` | Only statements that ran in this trace (sqlcommenter `traceparent`). |
| `--changed` | Only statements whose plan changed. |
| `--format text\|md\|json` | Default: `text`. |
| `--color auto\|always\|never` | Default: `auto`. |

## `explainsql top`

```text
explainsql top [OPTIONS]
```

Lists [a database's costliest statements](guide/top.md) from pg_stat_statements, and plans the one you pick.

| Option | What it does |
|---|---|
| `-d`, `--dbname DATABASE` | The database, as in connected mode. |
| `--limit N` | How many statements to list. Default: 20. |
| `--print` | Print the list instead of opening it. |
| `--format text\|md\|json` | The printed list's format. Default: `text`. |
| `--color auto\|always\|never` | Default: `auto`. |
| `--theme dark\|light` | Default: `dark`. |
| `--measure` | When trying parameter values, measure the plans where they differ. |
| `--allow-dml` | With `--measure`: also run statements that write, rolled back. |
| `--timeout SECONDS` | Stop each query after this many seconds. Default: 30. |

In the list: `Enter` plans the selected statement, `p` tries values for its parameters, `q` goes back or quits.

## `explainsql requests`

```text
explainsql requests [OPTIONS] FILES…
```

Groups the statements of [server logs into requests and finds the loops](guide/requests.md) (N+1), with the batched statement for each.

| Option | What it does |
|---|---|
| `-d`, `--dbname DATABASE` | Measure each loop's batched statement against its runs, and look up the foreign key behind it. |
| `--min-runs N` | The fewest runs of a statement in one request that make a loop. Default: 3. |
| `--gap MS` | Statements of a session without a trace or a transaction stay in one request while it is idle no longer than this. Default: 50. |
| `--limit N` | How many loops to show, and with `-d`, to measure. Default: 10. |
| `--runs N` | With `-d`: the median of this many measured runs of the batched statement. Default: 1. |
| `--allow-dml` | With `-d`: also measure loops of statements that write, rolled back. |
| `--timeout SECONDS` | With `-d`: stop a statement after this many seconds. Default: 30. |
| `--format text\|md\|json` | Default: `text`. |
| `--color auto\|always\|never` | Default: `auto`. |

## `explainsql anonymize`

```text
explainsql anonymize [OPTIONS] [FILE]
```

Prints the plans of `FILE` (or standard input) with [names and literal values replaced](guide/anonymize.md).

| Option | What it does |
|---|---|
| `--keep-names` | Keep the names of tables, columns and other objects; replace only literal values. |
| `--map FILE` | Write what each name and value became to this JSON file. |

## Environment variables

| Variable | Used for |
|---|---|
| `PGHOST`, `PGHOSTADDR`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`, `PGAPPNAME`, `PGSSLMODE`, `PGSSLROOTCERT`, `PGSERVICE` | Connection settings, as libpq reads them. |
| `PGPASSFILE`, `PGSERVICEFILE`, `PGSYSCONFDIR` | Where the password file, the service file and the system-wide service file are, when not in their default places. |
| `EXPLAINSQL_PAGER`, `PAGER` | In `--pager` mode, the pager for output that is not a plan. Default: `less -S`. |
| `VISUAL`, `EDITOR` | The editor `e` opens in the viewer. |
| `NO_COLOR` | Turns colors off in the viewer and in reports. |
| `COLORTERM`, `TERM` | How many colors the terminal supports. `TERM=dumb` disables the viewer. |
| `EXPLAINSQL_TEST_DATABASE_URL` | For development only: the database the connected-mode tests run against. |

## Files

| File | What it is |
|---|---|
| `explainsql.lock` | The locked plans of [`explainsql check`](guide/ci.md), meant to be committed. |
| `~/.pgpass` (`%APPDATA%\postgresql\pgpass.conf` on Windows) | Passwords, as for psql. Ignored when other users can read it. |
| `~/.pg_service.conf` | Connection services, as for psql. |

## Exit codes

| Command | 0 | 1 | 2 |
|---|---|---|---|
| `explainsql`, `diff`, `logs`, `top`, `requests`, `anonymize` | Success | An error, or with `--fail-on`, a finding at least that severe | |
| `check` | Every plan passed | A plan failed | The check could not run |
