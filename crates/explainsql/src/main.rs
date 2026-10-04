//! The `explainsql` command-line tool.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;
use std::{env, fs, io};

use explainsql_core::ir::{Node, Plan};

const USAGE: &str = "\
Usage: explainsql --debug-parse [--json] [FILE]
       explainsql --version

Reads a PostgreSQL EXPLAIN plan from FILE or standard input and prints what
the parser understood: the plan tree, the statement summary and any warnings.
The plan can be JSON or text, and may still be wrapped in psql output, an
auto_explain log entry or a Markdown code fence.

  --json   print the parsed plan as JSON instead
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let version = env!("CARGO_PKG_VERSION");
    match args.first().map(String::as_str) {
        Some("--debug-parse") => {}
        None | Some("-h" | "--help") => {
            print!(
                "explainsql {version}: in early development. See https://github.com/onplt/explain-sql\n\n{USAGE}"
            );
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("explainsql {version}");
            return ExitCode::SUCCESS;
        }
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
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
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
        Ok(plan) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&plan).expect("the IR serializes")
                );
            } else {
                print_plan(&plan);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
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

fn print_plan(plan: &Plan) {
    let wrappers: Vec<String> = plan
        .source
        .wrappers
        .iter()
        .map(|wrapper| format!("{wrapper:?}"))
        .collect();
    println!(
        "format: {:?}{}",
        plan.source.format,
        if wrappers.is_empty() {
            String::new()
        } else {
            format!(" (inside {})", wrappers.join(" > "))
        }
    );
    for (depth, node) in plan.walk() {
        println!("{}{}", "  ".repeat(depth), describe(node));
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
        println!("statement: {}", facts.join("; "));
    }
    if plan.warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings:");
        for warning in &plan.warnings {
            match warning.line {
                Some(line) => println!("  line {line}: {}", warning.message),
                None => println!("  {}", warning.message),
            }
        }
    }
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
