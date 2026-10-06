//! The `explainsql` command-line tool.

use std::io::{IsTerminal, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::{env, fs, io};

mod check;
mod connected;
mod logs;
mod params;
mod top;

use clap::{Args, Parser, Subcommand, ValueEnum};
use explainsql_core::Analysis;
use explainsql_core::ir::{Node, Plan};
use explainsql_core::report;

/// Find out why a PostgreSQL query is slow, from its EXPLAIN plan.
///
/// Reads a plan from FILE or standard input: JSON or text, as EXPLAIN prints
/// it or still wrapped in psql output, a server log entry, cells copied from
/// a GUI client or a Markdown code fence. Prints where the time went and what
/// to do about it.
///
/// In a terminal, the plan opens in an interactive viewer; press ? there for
/// the keys. Elsewhere, or with --print, a report is printed.
///
/// For the most useful report, capture the plan with
/// EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS).
///
/// Connected mode runs a query itself: explainsql -d "$DATABASE_URL" -f
/// slow.sql. It shows the estimated plan, then runs EXPLAIN ANALYZE in a
/// transaction that is always rolled back, READ ONLY unless --allow-dml.
/// With --params, a statement with parameters ($1, or ? as in JDBC) is
/// prepared as an application runs it, and the plans its values get are
/// compared with the generic plan.
///
/// explainsql diff BEFORE AFTER compares two plans of the same statement;
/// explainsql check checks plans in continuous integration; explainsql logs
/// tells when the plans in server logs changed; explainsql anonymize
/// prepares a plan for sharing; explainsql top lists a database's
/// costliest statements from pg_stat_statements.
#[derive(Parser)]
#[command(
    name = "explainsql",
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    task: Option<Task>,

    /// The plan file; standard input when missing or `-`.
    file: Option<String>,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Print a report instead of opening the interactive viewer, which is
    /// what happens anyway when the output is not a terminal.
    #[arg(long)]
    print: bool,

    /// Show a sample plan instead of reading one.
    #[arg(long, conflicts_with = "file")]
    demo: bool,

    /// Act as psql's pager (PSQL_PAGER='explainsql --pager'): open plans in
    /// the viewer and pass any other output on to $EXPLAINSQL_PAGER, $PAGER
    /// or `less -S`.
    #[arg(long, conflicts_with_all = ["file", "demo"])]
    pager: bool,

    /// The terminal's background, for the viewer's colors.
    #[arg(long, value_enum, default_value_t = Theme::Dark)]
    theme: Theme,

    /// When to color the text report.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,

    /// Print what the parser understood instead of the analysis, to check
    /// how a plan was read (with --format json: the parsed plan as JSON).
    #[arg(long)]
    debug_parse: bool,

    /// Connected mode: the database, as a URL (postgresql://user@host/db),
    /// key=value settings or a name. PG* variables, the service file and
    /// ~/.pgpass apply as in psql.
    #[arg(short = 'd', long, value_name = "DATABASE")]
    dbname: Option<String>,

    /// Connected mode: run the query in this file.
    #[arg(short = 'f', long, value_name = "FILE", conflicts_with_all = ["file", "demo", "pager"])]
    query_file: Option<String>,

    /// Connected mode: run this query.
    #[arg(short = 'c', long, value_name = "SQL", conflicts_with_all = ["file", "demo", "pager", "query_file"])]
    command: Option<String>,

    /// Connected mode: also run statements that modify data or lock rows.
    /// They run inside a transaction that is rolled back, but sequences,
    /// dblink calls and other effects outside the database are not undone.
    #[arg(long)]
    allow_dml: bool,

    /// Connected mode: to test a suggested index without HypoPG, build it
    /// inside a transaction that is rolled back. Building blocks writes to
    /// the table while it runs.
    #[arg(long)]
    allow_ddl: bool,

    /// Connected mode, with --print: test each suggested index (with
    /// HypoPG, or with --allow-ddl by building it) and report before and
    /// after.
    #[arg(long)]
    prove: bool,

    /// Connected mode, with --print: ask the database why the planner
    /// chose its plan for the slowest nodes, or for the scans of TABLE (a
    /// table or index name). It plans the statement again with the choice
    /// taken away (enable_seqscan = off, enable_nestloop = off) or with more
    /// work_mem, and compares. In the viewer, press y on a node instead.
    #[arg(long, value_name = "TABLE", num_args = 0..=1, default_missing_value = "")]
    why_not: Option<String>,

    /// Connected mode: the statement takes parameters ($1, or ? as in
    /// JDBC). Prepare it as an application does, try values from the
    /// columns' statistics and common LIMIT and OFFSET row counts, and
    /// compare the plan each gets with the generic plan, which PostgreSQL
    /// may switch to after five executions. Prints a report.
    #[arg(long, conflicts_with_all = ["why_not", "prove"])]
    params: bool,

    /// Connected mode: with --params, the value of a parameter, as 1=pending
    /// for $1, tried instead of values from the statistics. Repeat it for
    /// each parameter to give; implies --params.
    #[arg(
        long,
        value_name = "N=VALUE",
        value_parser = params::binding,
        conflicts_with_all = ["why_not", "prove"]
    )]
    bind: Vec<(usize, String)>,

    /// Connected mode: measure the alternatives of --why-not (and of y in
    /// the viewer), and the plans of --params where they differ, with
    /// EXPLAIN ANALYZE rather than only estimating them. Every run is
    /// rolled back.
    #[arg(long)]
    measure: bool,

    /// Connected mode: how many measured runs to compare for --prove and
    /// --measure, each side after one run that only warms the cache. The
    /// median counts.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=20))]
    runs: u16,

    /// Connected mode: stop a run after this many seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    timeout: u64,

    /// Connected mode: show the estimated plan only, without running the
    /// query.
    #[arg(long)]
    no_analyze: bool,

    /// With a printed report: exit with 1 when a finding is at least this
    /// severe, as a check in a script or CI.
    #[arg(long, value_enum, value_name = "SEVERITY")]
    fail_on: Option<Severity>,
}

#[derive(Subcommand)]
enum Task {
    /// Compare two plans of the same statement, node by node: which scans
    /// read their table another way, which joins changed method or order,
    /// which nodes came or went, and how the work of each node changed.
    Diff(DiffArgs),
    /// Check plans in continuous integration: each plan against its
    /// findings and against the plan locked for it, with exit code 0 when
    /// every plan passed, 1 when one failed and 2 on an error.
    ///
    /// Without -d, PATHS are plan files. With -d, they are SQL files, each
    /// run in a transaction that is rolled back, READ ONLY unless
    /// --allow-dml. Directories are searched for both. A plan fails when it
    /// is worse than its locked plan by pages (by the estimated cost when
    /// not run): time alone, for the same pages, does not fail it. --update
    /// locks the plans as they are.
    Check(CheckArgs),
    /// Read auto_explain plans from server logs: which plans each statement
    /// got, when its plan changed, what changed and what it cost, the
    /// costliest change first.
    ///
    /// FILES are server logs with auto_explain entries, plans in JSON or
    /// text: stderr with any log_line_prefix, csvlog or jsonlog; `-` reads
    /// standard input. Statements are told apart by their query identifier
    /// (compute_query_id, logged with auto_explain.log_verbose), or else by
    /// their text without literal values. sqlcommenter tags in the text say
    /// where in the application a statement comes from.
    Logs(LogsArgs),
    /// Replace what a plan tells about the data and the schema, to share it
    /// in a bug report or an issue: names of tables, indexes, columns and
    /// other objects become table_a, index_a, column_a, …, and literal
    /// values become 'value_a' or other numbers, the same way everywhere
    /// they appear. Names that differ only in their numbers, as partitions
    /// do, stay alike (table_b_1, table_b_2). Node types, estimates,
    /// timings and buffers stay, so the plan reads and analyzes as before.
    ///
    /// Prints the plans of FILE, in the format they were written in, without
    /// what surrounded them (psql output, log lines, code fences). Function
    /// and type names, keywords and $n parameters are kept.
    Anonymize(AnonymizeArgs),
    /// List a database's costliest statements, from pg_stat_statements,
    /// and plan one of them.
    ///
    /// In a terminal, Enter shows the plan of the selected statement,
    /// estimated and without running it: on PostgreSQL 16 or later, a
    /// statement with parameters ($1, as pg_stat_statements writes
    /// constants) gets its generic plan (EXPLAIN GENERIC_PLAN). p tries
    /// values for its parameters, as --params does, and shows the report;
    /// before PostgreSQL 16, so does Enter. Elsewhere, or with --print, the
    /// list is printed.
    ///
    /// The extension must be installed in the database (CREATE EXTENSION
    /// pg_stat_statements) and loaded (shared_preload_libraries). Other
    /// users' statements need the pg_read_all_stats role.
    Top(TopArgs),
}

#[derive(Args)]
struct AnonymizeArgs {
    /// The plan file; standard input when missing or `-`.
    file: Option<String>,

    /// Keep the names of tables, columns and other objects; replace only
    /// literal values.
    #[arg(long)]
    keep_names: bool,

    /// Write what each name and value became to this JSON file, to read
    /// answers about the anonymized plan back. Keep it to yourself: it
    /// holds the originals.
    #[arg(long, value_name = "FILE")]
    map: Option<String>,
}

#[derive(Args)]
struct TopArgs {
    /// The database, as -d in connected mode; PG* variables, the service
    /// file and ~/.pgpass apply as in psql.
    #[arg(short = 'd', long, value_name = "DATABASE")]
    dbname: Option<String>,

    /// How many statements to list.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=1000))]
    limit: u16,

    /// Print the list instead of opening it, which is what happens anyway
    /// when the output is not a terminal.
    #[arg(long)]
    print: bool,

    /// Format of the printed list.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// When to color the printed list.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,

    /// The terminal's background, for the list's and the viewer's colors.
    #[arg(long, value_enum, default_value_t = Theme::Dark)]
    theme: Theme,

    /// When trying values: measure the plans where they differ, with
    /// EXPLAIN ANALYZE in a transaction that is rolled back, rather than
    /// only estimate them.
    #[arg(long)]
    measure: bool,

    /// With --measure: also run statements that modify data or lock rows,
    /// in a transaction that is rolled back.
    #[arg(long)]
    allow_dml: bool,

    /// Stop each query after this many seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    timeout: u64,
}

#[derive(Args)]
struct LogsArgs {
    /// Server logs with auto_explain entries; `-` for standard input.
    #[arg(required = true, value_name = "FILES")]
    files: Vec<String>,

    /// Only entries from this time on, written as the log prints times
    /// (2026-10-06 06:00), or 30m, 24h, 7d back from the last entry.
    #[arg(long, value_name = "TIME")]
    since: Option<String>,

    /// Only entries up to this time.
    #[arg(long, value_name = "TIME")]
    until: Option<String>,

    /// Only the statement with this query identifier, or whose text
    /// contains this.
    #[arg(long, value_name = "ID|TEXT")]
    query: Option<String>,

    /// Only statements that ran in this trace: the trace id of a
    /// sqlcommenter traceparent tag.
    #[arg(long, value_name = "TRACE_ID")]
    trace: Option<String>,

    /// Only statements whose plan changed.
    #[arg(long)]
    changed: bool,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// When to color the text report.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

#[derive(Args)]
struct CheckArgs {
    /// Plan files, or with -d, SQL files; directories are searched for
    /// *.json and *.txt plans, or *.sql statements.
    #[arg(required = true, value_name = "PATHS")]
    paths: Vec<String>,

    /// Run the SQL files against this database (as -d in connected mode).
    #[arg(short = 'd', long, value_name = "DATABASE")]
    dbname: Option<String>,

    /// Also fail a plan with a finding at least this severe.
    #[arg(long, value_enum, value_name = "SEVERITY")]
    fail_on: Option<Severity>,

    /// Also fail a plan whose shape changed from its locked plan, even when
    /// it is not worse.
    #[arg(long)]
    strict: bool,

    /// The file of locked plans.
    #[arg(long, value_name = "FILE", default_value = "explainsql.lock")]
    lock: String,

    /// Lock the plans as they are now, rather than check them: to start,
    /// or to accept a change. Other plans in the file stay as they are.
    #[arg(long, conflicts_with_all = ["fail_on", "strict", "prove"])]
    update: bool,

    /// With -d: test the suggested indexes of each plan that failed with
    /// HypoPG, and report before and after.
    #[arg(long, requires = "dbname")]
    prove: bool,

    /// With -d: plan the statements without running them.
    #[arg(long, requires = "dbname")]
    no_analyze: bool,

    /// With -d: also run statements that modify data or lock rows, in a
    /// transaction that is rolled back.
    #[arg(long, requires = "dbname")]
    allow_dml: bool,

    /// With -d: stop a statement after this many seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    timeout: u64,

    /// Report format: sarif for code scanning, md for a pull request
    /// comment.
    #[arg(long, value_enum, default_value_t = CheckFormat::Text)]
    format: CheckFormat,

    /// Also write the report as SARIF to this file, for code scanning
    /// beside a report in another format.
    #[arg(long, value_name = "FILE", conflicts_with = "update")]
    sarif: Option<String>,

    /// When to color the text report.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CheckFormat {
    /// For a terminal.
    Text,
    /// Markdown, for a pull request comment.
    Md,
    /// JSON, for other programs.
    Json,
    /// SARIF 2.1.0, for code scanning (GitHub and others).
    Sarif,
}

/// How severe a finding is.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Severity {
    Low,
    Medium,
    High,
}

impl From<Severity> for explainsql_core::rules::Severity {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Low => explainsql_core::rules::Severity::Low,
            Severity::Medium => explainsql_core::rules::Severity::Medium,
            Severity::High => explainsql_core::rules::Severity::High,
        }
    }
}

#[derive(Args)]
struct DiffArgs {
    /// The plan before: a file, or `-` for standard input.
    before: String,

    /// The plan after. Without it, BEFORE must hold both plans, one after
    /// the other: pasted text, a JSON array, Markdown code fences or log
    /// entries.
    after: Option<String>,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// When to color the text report.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// For a terminal.
    Text,
    /// Markdown, for an issue or a pull request.
    Md,
    /// JSON, for other programs.
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Theme {
    Dark,
    Light,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Color {
    /// When the output is a terminal and NO_COLOR is not set.
    Auto,
    Always,
    Never,
}

/// Appends a formatted line to a `String`.
macro_rules! push_line {
    ($out:expr, $($arg:tt)*) => {{
        $out.push_str(&format!($($arg)*));
        $out.push('\n');
    }};
}

/// The plan `--demo` shows: a nested loop that rescans a table, with two
/// findings.
const DEMO: &str = include_str!("../demo/plan.txt");

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.task {
        Some(Task::Diff(args)) => return diff(args),
        Some(Task::Check(args)) => return check::run(args),
        Some(Task::Logs(args)) => return logs::run(args),
        Some(Task::Anonymize(args)) => return anonymize(args),
        Some(Task::Top(args)) => return top::run(args),
        None => {}
    }
    if cli.query_file.is_some() || cli.command.is_some() {
        return connected::run(&cli);
    }
    if cli.dbname.is_some() {
        eprintln!("error: give the query to run with -f FILE or -c SQL");
        return ExitCode::FAILURE;
    }
    if cli.why_not.is_some() || cli.measure || cli.params || !cli.bind.is_empty() {
        eprintln!(
            "error: --why-not, --params and --measure ask the database: give the query to run with -d DATABASE and -f FILE or -c SQL"
        );
        return ExitCode::FAILURE;
    }
    let input = if cli.demo {
        DEMO.to_owned()
    } else {
        match read_input(cli.file.as_deref(), cli.pager) {
            Ok(input) => input,
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        }
    };
    let plan = match explainsql_core::parse(&input) {
        Ok(plan) => plan,
        Err(_) if cli.pager => return page(&input),
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    // A pager whose output is not a terminal passes everything through.
    if cli.pager && !io::stdout().is_terminal() {
        return emit(&input);
    }
    if cli.debug_parse {
        return match cli.format {
            Format::Json => emit(&format!(
                "{}\n",
                serde_json::to_string_pretty(&plan).expect("the IR serializes")
            )),
            Format::Text | Format::Md => emit(&render(&plan)),
        };
    }
    let analysis = explainsql_core::analyze(&plan);
    if cli.format == Format::Text && !cli.print && interactive() {
        match explainsql_tui::run(plan.clone(), analysis.clone(), viewer_options(&cli)) {
            Ok(()) => return ExitCode::SUCCESS,
            // Without a usable terminal, the report is the next best thing.
            Err(error) => {
                eprintln!("explainsql: cannot open the viewer ({error}); printing a report")
            }
        }
    }
    with_findings(&cli, &analysis, emit(&report_for(&cli, &plan, &analysis)))
}

/// The exit code after a printed report: 1 when `--fail-on` is given and a
/// finding is at least that severe.
pub(crate) fn with_findings(cli: &Cli, analysis: &Analysis, code: ExitCode) -> ExitCode {
    let Some(threshold) = cli.fail_on else {
        return code;
    };
    let threshold = explainsql_core::rules::Severity::from(threshold);
    if code == ExitCode::SUCCESS
        && analysis
            .findings
            .iter()
            .any(|finding| finding.severity >= threshold)
    {
        ExitCode::FAILURE
    } else {
        code
    }
}

/// `explainsql anonymize`: prints the plans with their names and values
/// replaced.
fn anonymize(args: &AnonymizeArgs) -> ExitCode {
    let input = match read_input(args.file.as_deref(), false) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let options = explainsql_core::anonymize::Options {
        keep_names: args.keep_names,
    };
    let anonymized = match explainsql_core::anonymize::anonymize(&input, options) {
        Ok(anonymized) => anonymized,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = &args.map {
        let mut json =
            serde_json::to_string_pretty(&anonymized.mapping).expect("the mapping serializes");
        json.push('\n');
        if let Err(error) = fs::write(path, json) {
            eprintln!("error: {path}: {error}");
            return ExitCode::FAILURE;
        }
    }
    emit(&anonymized.text)
}

/// `explainsql diff`: reads both plans and prints how they differ.
fn diff(args: &DiffArgs) -> ExitCode {
    let read =
        |path: &str| read_input(Some(path), false).map_err(|error| format!("error: {error}"));
    let plans = match &args.after {
        Some(after) => read(&args.before).and_then(|before| {
            let after = read(after)?;
            let parse = |text: &str, which: &str| {
                explainsql_core::parse(text).map_err(|error| format!("error: {which}: {error}"))
            };
            Ok((parse(&before, "the plan before")?, parse(&after, "the plan after")?))
        }),
        None => read(&args.before).and_then(|input| {
            let plans = explainsql_core::parse_all(&input).map_err(|error| format!("error: {error}"))?;
            if plans.len() > 2 {
                eprintln!(
                    "explainsql: the input holds {} plans; comparing the first two",
                    plans.len()
                );
            }
            let mut plans = plans.into_iter();
            match (plans.next(), plans.next()) {
                (Some(before), Some(after)) => Ok((before, after)),
                _ => Err(
                    "error: the input holds one plan; give the plan after as a second file, or both plans in one input"
                        .to_owned(),
                ),
            }
        }),
    };
    let (before, after) = match plans {
        Ok(plans) => plans,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    for (which, plan) in [("before", &before), ("after", &after)] {
        for warning in &plan.warnings {
            match warning.line {
                Some(line) => eprintln!(
                    "explainsql: the plan {which}: line {line}: {}",
                    warning.message
                ),
                None => eprintln!("explainsql: the plan {which}: {}", warning.message),
            }
        }
    }
    let diff = explainsql_core::diff::diff(&before, &after);
    emit(&match args.format {
        Format::Text => report::diff_text(&before, &after, &diff, use_color(args.color)),
        Format::Md => report::diff_markdown(&before, &after, &diff),
        Format::Json => report::diff_json(&before, &after, &diff),
    })
}

fn viewer_options(cli: &Cli) -> explainsql_tui::Options {
    explainsql_tui::Options {
        background: match cli.theme {
            Theme::Dark => explainsql_tui::Background::Dark,
            Theme::Light => explainsql_tui::Background::Light,
        },
        depth: None,
    }
}

/// The report in the format asked for.
fn report_for(cli: &Cli, plan: &Plan, analysis: &Analysis) -> String {
    match cli.format {
        Format::Text => report::text(plan, analysis, use_color(cli.color)),
        Format::Md => report::markdown(plan, analysis),
        Format::Json => report::json(plan, analysis),
    }
}

/// Whether to color a text report.
fn use_color(color: Color) -> bool {
    match color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => {
            io::stdout().is_terminal()
                && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                && env::var("TERM").map_or(true, |term| term != "dumb")
        }
    }
}

/// Whether the viewer can run: the output is a terminal that can show it.
fn interactive() -> bool {
    io::stdout().is_terminal() && env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Shows text that is not a plan the way a pager would: through
/// $EXPLAINSQL_PAGER, $PAGER or `less -S`, or straight to standard output
/// when none of them runs.
fn page(text: &str) -> ExitCode {
    if !io::stdout().is_terminal() {
        return emit(text);
    }
    let commands = [env::var("EXPLAINSQL_PAGER").ok(), env::var("PAGER").ok()]
        .into_iter()
        .flatten()
        .map(|command| command.trim().to_owned())
        // Never ourselves, which would loop.
        .filter(|command| !command.is_empty() && !command.contains("explainsql"))
        .chain(std::iter::once("less -S".to_owned()));
    for command in commands {
        let shell = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", &command])
                .stdin(Stdio::piped())
                .spawn()
        } else {
            Command::new("sh")
                .args(["-c", &command])
                .stdin(Stdio::piped())
                .spawn()
        };
        let Ok(mut child) = shell else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            // The pager may quit before reading everything.
            let _ = stdin.write_all(text.as_bytes());
        }
        return match child.wait() {
            Ok(status) if status.success() => ExitCode::SUCCESS,
            // The shell could not find the command: try the next one.
            Ok(status) if status.code() == Some(127) => continue,
            _ => ExitCode::FAILURE,
        };
    }
    emit(text)
}

/// Writes to standard output. A reader that stops early, such as `head`,
/// is not an error.
fn emit(text: &str) -> ExitCode {
    match io::stdout().lock().write_all(text.as_bytes()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
        _ => ExitCode::SUCCESS,
    }
}

fn read_input(file: Option<&str>, pager: bool) -> io::Result<String> {
    let bytes = match file {
        Some("-") | None => {
            if io::stdin().is_terminal() && !pager {
                eprintln!("Reading a plan from standard input; end with Ctrl-D.");
            }
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
            bytes
        }
        Some(path) => {
            fs::read(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?
        }
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// What the parser understood: the plan tree, the statement summary and
/// the warnings.
fn render(plan: &Plan) -> String {
    let mut out = String::new();
    let wrappers: Vec<String> = plan
        .source
        .wrappers
        .iter()
        .map(|wrapper| format!("{wrapper:?}"))
        .collect();
    push_line!(
        out,
        "format: {:?}{}",
        plan.source.format,
        if wrappers.is_empty() {
            String::new()
        } else {
            format!(" (inside {})", wrappers.join(" > "))
        }
    );
    for (depth, node) in plan.walk() {
        push_line!(out, "{}{}", "  ".repeat(depth), describe(node));
    }
    let summary = &plan.summary;
    let mut facts = Vec::new();
    if let Some(ms) = summary.planning_time {
        facts.push(format!("planning {ms:.3} ms"));
    }
    if let Some(ms) = summary.execution_time {
        facts.push(format!("execution {ms:.3} ms"));
    }
    for trigger in &summary.triggers {
        facts.push(format!(
            "trigger {}: {} calls{}",
            trigger
                .name
                .as_deref()
                .or(trigger.constraint.as_deref())
                .unwrap_or("?"),
            trigger.calls,
            trigger
                .time
                .map(|ms| format!(", {ms:.3} ms"))
                .unwrap_or_default()
        ));
    }
    if let Some(jit) = &summary.jit {
        facts.push(format!(
            "JIT: {} functions{}",
            jit.functions,
            jit.timing
                .map(|timing| format!(", {:.3} ms", timing.total))
                .unwrap_or_default()
        ));
    }
    if !summary.settings.is_empty() {
        let settings: Vec<String> = summary
            .settings
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        facts.push(format!("settings: {}", settings.join(", ")));
    }
    if !facts.is_empty() {
        push_line!(out, "statement: {}", facts.join("; "));
    }
    if plan.warnings.is_empty() {
        push_line!(out, "warnings: none");
    } else {
        push_line!(out, "warnings:");
        for warning in &plan.warnings {
            match warning.line {
                Some(line) => push_line!(out, "  line {line}: {}", warning.message),
                None => push_line!(out, "  {}", warning.message),
            }
        }
    }
    out
}

/// One line per node: its name, target, estimates and measurements.
fn describe(node: &Node) -> String {
    let mut text = String::new();
    if let Some(name) = &node.subplan_name {
        text.push_str(&format!("[{name}] "));
    }
    if node.parallel_aware {
        text.push_str("Parallel ");
    }
    text.push_str(&node.node_type);
    if let Some(join_type) = node.join_type.as_deref().filter(|&t| t != "Inner") {
        text.push_str(&format!(" ({join_type})"));
    }
    if let Some(strategy) = &node.strategy {
        text.push_str(&format!(" ({strategy})"));
    }
    if let Some(operation) = &node.operation {
        text.push_str(&format!(" ({operation})"));
    }
    if let Some(index) = &node.index_name {
        text.push_str(&format!(" using {index}"));
    }
    let object = node
        .relation_name
        .as_deref()
        .or(node.cte_name.as_deref())
        .or(node.function_name.as_deref());
    if let Some(object) = object {
        text.push_str(" on ");
        if let Some(schema) = &node.schema {
            text.push_str(&format!("{schema}."));
        }
        text.push_str(object);
    }
    if let Some(alias) = node.alias.as_deref().filter(|&alias| Some(alias) != object) {
        text.push_str(&format!(" {alias}"));
    }
    if let Some(estimates) = node.estimates {
        text.push_str(&format!(
            "  [estimated rows {} cost {:.2}]",
            estimates.rows, estimates.total_cost
        ));
    }
    if let Some(actuals) = node.actuals {
        if actuals.never_executed() {
            text.push_str("  [never executed]");
        } else {
            text.push_str(&format!(
                "  [actual rows {} × {} loops",
                actuals.rows, actuals.loops
            ));
            if let Some(ms) = actuals.total_time {
                text.push_str(&format!(", {ms:.3} ms per loop"));
            }
            text.push(']');
        }
    }
    text
}
