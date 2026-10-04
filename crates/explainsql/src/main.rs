//! The `explainsql` command-line tool.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::{env, fs, io};

use explainsql_core::ir::{Node, Plan};

const USAGE: &str = "\
Usage: explainsql --debug-parse [--json] [FILE]
       explainsql --version

Reads a PostgreSQL EXPLAIN plan from FILE or standard input and prints what
the parser understood: the plan tree, the statement summary and any warnings.
The plan can be JSON or text, and may still be wrapped in psql output, a
server log entry, cells copied from a GUI client or a Markdown code fence.

  --json   print the parsed plan as JSON instead
";

/// Appends a formatted line to a `String`.
macro_rules! push_line {
    ($out:expr, $($arg:tt)*) => {{
        $out.push_str(&format!($($arg)*));
        $out.push('\n');
    }};
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let version = env!("CARGO_PKG_VERSION");
    match args.first().map(String::as_str) {
        Some("--debug-parse") => {}
        None | Some("-h" | "--help") => {
            return emit(&format!(
                "explainsql {version}: in early development. See https://github.com/onplt/explain-sql\n\n{USAGE}"
            ));
        }
        Some("-V" | "--version") => return emit(&format!("explainsql {version}\n")),
        Some(other) => {
            eprintln!("error: unexpected argument `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let mut json = false;
    let mut file = None;
    for arg in &args[1..] {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return emit(USAGE),
            path if file.is_none() => file = Some(path.to_owned()),
            other => {
                eprintln!("error: unexpected argument `{other}`\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let input = match read_input(file.as_deref()) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match explainsql_core::parse(&input) {
        Ok(plan) if json => emit(&format!(
            "{}\n",
            serde_json::to_string_pretty(&plan).expect("the IR serializes")
        )),
        Ok(plan) => emit(&render(&plan)),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
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

/// The plan tree, the statement summary and the warnings, as text.
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
