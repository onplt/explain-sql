//! Development tasks for the ExplainSQL workspace: `cargo xtask <task>`.

mod fixtures;
mod scenario;

use std::process::ExitCode;

fn usage() -> String {
    let versions: Vec<String> = fixtures::DEFAULT_VERSIONS
        .iter()
        .map(u32::to_string)
        .collect();
    format!(
        "\
Usage: cargo xtask <task> [options]

Tasks:
  gen-fixtures      Regenerate the EXPLAIN fixture corpus (requires Docker)
      --versions <list>   PostgreSQL major versions, comma-separated (default: {})
      --only <list>       Only these scenarios, comma-separated (default: all)
      --keep-containers   Leave the containers running for debugging
  check-fixtures    Check that the committed corpus matches fixtures/scenarios
  help              Show this message
",
        versions.join(",")
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("gen-fixtures") => {
            fixtures::Options::parse(&args[1..]).and_then(|options| fixtures::generate(&options))
        }
        Some("check-fixtures") => fixtures::check(),
        Some("help" | "--help" | "-h") | None => {
            print!("{}", usage());
            Ok(())
        }
        Some(other) => Err(format!("unknown task `{other}`\n\n{}", usage())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
