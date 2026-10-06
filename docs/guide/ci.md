# Check plans in CI

Plans regress quietly. A new column in a `WHERE`, a dropped index in a migration, a rewritten ORM query: the tests still pass, and the slowdown shows up in production a week later. `explainsql check` makes plans part of continuous integration, so that a plan that got worse fails the build, like a failing test.

```sh
explainsql check -d "$DATABASE_URL" queries/ --update      # lock the plans as they are; commit explainsql.lock
explainsql check -d "$DATABASE_URL" queries/               # in CI: fail when a plan got worse
explainsql check -d "$DATABASE_URL" queries/ --fail-on high --prove --format md > comment.md
explainsql check plans/ --format sarif > explainsql.sarif  # captured plans, no database
```

## How it works

You keep the statements you care about in files, one statement per `.sql` file, anywhere in the repository. Each is checked twice: against its findings, and against the plan locked for it in `explainsql.lock`.

**What it reads.** With `-d`, SQL files (directories are searched for `*.sql`), each run as in [connected mode](connected.md): in a transaction that is rolled back, `READ ONLY` unless `--allow-dml`, with `EXPLAIN ANALYZE`, or with `EXPLAIN` alone under `--no-analyze`. Without `-d`, plan files in any form ExplainSQL reads (directories are searched for `*.json` and `*.txt`).

**The lock file.** `--update` writes each plan to `explainsql.lock` (`--lock FILE` for another name), under its path relative to the lock file's directory: its [shape](diff.md#plan-shapes), its pages, its estimated cost, and the plan itself, with a JSON plan stored as JSON so that a change reads well in code review. Plans not checked in that run are kept as they are. Commit the file, and run `--update` again whenever you accept a change.

**When a plan fails.**

- It is worse than its locked plan by pages, by temporary files, or, when neither plan was run, by the planner's estimated cost, by more than 10%. Time alone never fails a plan: on a shared CI runner it changes from one run to the next for the same work, so a change in time is reported as a note.
- With `--fail-on SEVERITY`, a finding at least that severe fails it, locked or not.
- With `--strict`, a plan whose shape changed fails even when it is not worse. The plan becomes a contract, changed on purpose with `--update`.

A plan that is not in the lock file yet is new, and fails only on its findings.

**What it says.** For each plan that failed: why, what changed in the plan (as [`explainsql diff`](diff.md) tells it), and the suggested fix. With `--prove` and HypoPG installed in the database, each suggested index is tested and reported before and after.

**Exit codes.** 0 when every plan passed, 1 when at least one failed, and 2 when the check could not run. A file that cannot be read or run is reported on standard error and makes the exit code 2, after the other files are checked.

## Report formats

- `--format text` (the default): one line per plan with its shape, then why it failed, notes and fixes.
- `--format md`: a pull request comment, with the plans in a table and the diff of each one that failed or changed folded below. It starts with the line `<!-- explainsql check -->`, which is invisible in a rendered comment, and stays under GitHub's size limit for comments: when the plans do not fit, those that failed come first, then those that changed, and the rest are counted.
- `--format sarif`: SARIF 2.1.0 for code scanning. Each finding, and each plan worse than its lock, is a result on its file: an error when it fails the plan, otherwise a warning or a note by severity.
- `--format json`: every check with its findings and advice, for other programs.

`--sarif FILE` writes the SARIF report as well, alongside a report in another format, so one run gives you both.

## The GitHub Action

The repository is also a GitHub Action. It installs ExplainSQL, runs `explainsql check`, and writes the report on the pull request as a comment. Later runs update that comment in place instead of adding new ones: a check that fails posts or updates it, and a check that passes edits an existing comment to say so, without posting a new one. The report also goes to the job summary, and the job fails when the check fails.

```yaml
on: pull_request
permissions:
  contents: read
  pull-requests: write        # for the comment
  security-events: write      # only with upload-sarif
jobs:
  plans:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_PASSWORD: postgres
        ports: ["5432:5432"]
        options: --health-cmd pg_isready --health-interval 5s --health-retries 10
    steps:
      - uses: actions/checkout@v5
      - run: psql "$DATABASE_URL" -f schema.sql   # the tables, and data shaped like production's
        env:
          DATABASE_URL: postgresql://postgres:postgres@localhost:5432/postgres
      - uses: onplt/explain-sql@v0.3.0
        with:
          paths: queries/
          database-url: postgresql://postgres:postgres@localhost:5432/postgres
          fail-on: high
          upload-sarif: true
```

Without `database-url`, `paths` are captured plan files and no database is needed.

| Input | Default | What it does |
|---|---|---|
| `paths` | (required) | Plan files, or SQL files with `database-url`; directories are searched. Separated by spaces or new lines. |
| `database-url` | | Run the SQL files against this database. |
| `lock` | `explainsql.lock` | The file of locked plans. |
| `fail-on` | | Also fail a plan with a finding at least this severe: `high`, `medium` or `low`. |
| `strict` | `false` | Also fail a plan whose shape changed. |
| `args` | | More arguments for `explainsql check`, such as `--prove`, `--no-analyze` or `--allow-dml`. |
| `comment` | `true` | Comment on the pull request. |
| `comment-key` | `default` | Tells this check's comment from another's, when one workflow checks several sets of plans. |
| `upload-sarif` | `false` | Upload the report to code scanning (needs `security-events: write`). |
| `version` | the action's | The ExplainSQL release to install, such as `0.3.0`. By default, the release the action is referenced by (`@v0.3.0`), or the latest. |
| `binary` | | An ExplainSQL binary to use instead of installing a release. |
| `github-token` | `github.token` | The token that writes the comment (needs `pull-requests: write`). |

The outputs are `result` (`passed`, `failed` or `error`), `exit-code` (0, 1 or 2), and the paths of the reports, `report` (Markdown) and `sarif`. The action runs on Linux and macOS runners and needs ExplainSQL 0.2.0 or later.

A pull request from a fork gets no comment, because its token cannot write one, and `pull_request_target`, whose token can, would run the fork's code with your repository's secrets. Its report is still in the job summary.

### Without the action

If you prefer plain steps, or another CI system, run the binary yourself:

```yaml
- name: Check the plans
  run: explainsql check -d "$DATABASE_URL" queries/ --fail-on high --format sarif > explainsql.sarif
- name: Show them in code scanning
  if: always()
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: explainsql.sarif
```

For a single plan, the main command takes `--fail-on` too: `explainsql --print --fail-on high plan.json` exits with 1 when a finding is at least that severe.

## Make the data realistic

A plan from a database with a handful of rows says very little: the planner reads tiny tables whole, whatever indexes you have. Check against data shaped like production's, even if it is generated. From PostgreSQL 18 you can also restore production's statistics into the CI database (`pg_restore_relation_stats`, `pg_restore_attribute_stats`), although the planner still sees the actual size of each table on disk.
