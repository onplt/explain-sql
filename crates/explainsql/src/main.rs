//! The `explainsql` command-line tool.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::{env, fs, io};

use clap::{Parser, ValueEnum};
use explainsql_core::ir::{Node, Plan};
use explainsql_core::report;

/// Find out why a PostgreSQL query is slow, from its EXPLAIN plan.
///
/// Reads a plan from FILE or standard input: JSON or text, as EXPLAIN prints
/// it or still wrapped in psql output, a server log entry, cells copied from
/// a GUI client or a Markdown code fence. Prints where the time went and what
/// to do about it.
///
/// For the most useful report, capture the plan with
/// EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS).
#[derive(Parser)]
#[command(name = "explainsql", version)]
struct Cli {
    /// The plan file; standard input when missing or `-`.
    file: Option<String>,

    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Print a report instead of opening the interactive viewer. The viewer
    /// is not built yet, so this is currently always the case.
    #[arg(long)]
    print: bool,

    /// When to color the text report.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,

    /// Print what the parser understood instead of the analysis, to check
    /// how a plan was read (with --format json: the parsed plan as JSON).
    #[arg(long)]
    debug_parse: bool,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let input = match read_input(cli.file.as_deref()) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let plan = match explainsql_core::parse(&input) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
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
    let output = match cli.format {
        Format::Text => {
            let color = match cli.color {
                Color::Always => true,
                Color::Never => false,
                Color::Auto => {
                    io::stdout().is_terminal()
                        && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                        && env::var("TERM").map_or(true, |term| term != "dumb")
                }
            };
            report::text(&plan, &analysis, color)
        }
        Format::Md => report::markdown(&plan, &analysis),
        Format::Json => report::json(&plan, &analysis),
    };
    emit(&output)
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

fn read_input(file: Option<&str>) -> io::Result<String> {
    let bytes = match file {
        Some("-") | None => {
            if io::stdin().is_terminal() {
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
