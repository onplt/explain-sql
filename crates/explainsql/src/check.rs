//! `explainsql check`: plans in continuous integration. Reads plan files,
//! or with `-d` runs SQL files, checks each plan against its findings and
//! against the plan locked for it in `explainsql.lock`, prints a report and
//! exits with 0 when every plan passed, 1 when one failed, 2 on an error.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use explainsql_core::check::{self, Lock, Policy, Status};
use explainsql_core::report;
use explainsql_db::{Database, Mode, Safety, Settings};

use crate::{CheckArgs, CheckFormat, connected, emit, use_color};

/// The exit code for a check that found a problem.
const FAILED: u8 = 1;
/// The exit code for a check that could not run.
const ERROR: u8 = 2;

pub fn run(args: &CheckArgs) -> ExitCode {
    match try_run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(ERROR)
        }
    }
}

fn try_run(args: &CheckArgs) -> Result<ExitCode, String> {
    let connected = args.dbname.is_some();
    let files = inputs(&args.paths, connected)?;
    if files.is_empty() {
        return Err(format!(
            "no {} in {}",
            if connected {
                "SQL files (*.sql)"
            } else {
                "plan files (*.json, *.txt)"
            },
            args.paths.join(", ")
        ));
    }
    let lock_path = PathBuf::from(&args.lock);
    let mut lock = match fs::read_to_string(&lock_path) {
        Ok(text) => {
            Lock::read(&text).map_err(|error| format!("{}: {error}", lock_path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Lock::default(),
        Err(error) => return Err(format!("{}: {error}", lock_path.display())),
    };
    let base = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let policy = Policy {
        fail_on: args.fail_on.map(crate::Severity::into),
        strict: args.strict,
    };
    let database = match &args.dbname {
        Some(name) => {
            let settings = Settings::resolve(Some(name))?;
            Some(Database::connect(&settings).map_err(|error| error.to_string())?)
        }
        None => None,
    };
    let safety = Safety {
        allow_dml: args.allow_dml,
        allow_ddl: false,
        timeout: Duration::from_secs(args.timeout.max(1)),
    };
    let mode = if args.no_analyze {
        Mode::Estimate
    } else {
        Mode::Analyze
    };

    let mut checked = Vec::new();
    let mut errors = 0;
    for file in &files {
        let name = name_of(file, &base);
        let read = || fs::read_to_string(file).map_err(|error| error.to_string());
        let captured = match &database {
            Some(db) => read().and_then(|sql| {
                let json = db.explain(&sql, mode, safety).map_err(|e| e.to_string())?;
                let plan = explainsql_core::parse(&json).map_err(|e| e.to_string())?;
                let (analysis, _) = connected::analyzed(db, &plan);
                Ok((json, plan, analysis, Some(sql)))
            }),
            None => read().and_then(|text| {
                let plan = explainsql_core::parse(&text).map_err(|e| e.to_string())?;
                let analysis = explainsql_core::analyze(&plan);
                Ok((text, plan, analysis, None))
            }),
        };
        let (text, plan, analysis, sql) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                eprintln!("explainsql: {name}: {error}");
                errors += 1;
                continue;
            }
        };
        for warning in &plan.warnings {
            eprintln!("explainsql: {name}: {}", warning.message);
        }
        if args.update {
            lock.lock(&name, &text, &plan);
            continue;
        }
        let baseline = match lock.plan(&name) {
            Some(Ok(plan)) => Some(plan),
            Some(Err(error)) => {
                eprintln!("explainsql: {name}: {error}");
                errors += 1;
                continue;
            }
            None => None,
        };
        let mut item = check::check(&name, plan, analysis, baseline, policy);
        // A failed plan with a database at hand: test what would fix it.
        if let (true, Status::Failed, Some(db), Some(sql)) =
            (args.prove, item.status, &database, &sql)
        {
            connected::prove_all(db, sql, &mut item.analysis, 1, safety);
        }
        checked.push(item);
    }

    if args.update {
        fs::write(&lock_path, lock.write())
            .map_err(|error| format!("{}: {error}", lock_path.display()))?;
        eprintln!(
            "explainsql: locked {} plan{} in {}",
            files.len() - errors,
            if files.len() - errors == 1 { "" } else { "s" },
            lock_path.display()
        );
        return Ok(if errors > 0 {
            ExitCode::from(ERROR)
        } else {
            ExitCode::SUCCESS
        });
    }

    let output = match args.format {
        CheckFormat::Text => report::check_text(&checked, use_color(args.color)),
        CheckFormat::Md => report::check_markdown(&checked),
        CheckFormat::Json => report::check_json(&checked),
        CheckFormat::Sarif => report::check_sarif(&checked),
    };
    if emit(&output) != ExitCode::SUCCESS {
        return Ok(ExitCode::from(ERROR));
    }
    Ok(if errors > 0 {
        ExitCode::from(ERROR)
    } else if !check::passed(&checked) {
        ExitCode::from(FAILED)
    } else {
        ExitCode::SUCCESS
    })
}

/// The files to check: those given, and in the directories given, the SQL
/// files (with a database) or the plan files, in order.
fn inputs(paths: &[String], sql: bool) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in paths {
        let path = PathBuf::from(path);
        if path.is_dir() {
            let mut found = Vec::new();
            collect(&path, sql, &mut found)?;
            found.sort();
            files.extend(found);
        } else if path.is_file() {
            files.push(path);
        } else {
            return Err(format!("{}: no such file or directory", path.display()));
        }
    }
    Ok(files)
}

fn collect(dir: &Path, sql: bool, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("{}: {error}", dir.display()))?
            .path();
        if path.is_dir() {
            collect(&path, sql, found)?;
            continue;
        }
        let extension = path.extension().and_then(|extension| extension.to_str());
        let wanted = if sql {
            matches!(extension, Some("sql"))
        } else {
            matches!(extension, Some("json" | "txt"))
        };
        if wanted {
            found.push(path);
        }
    }
    Ok(())
}

/// A file's name in the lock: its path from the lock file's directory,
/// with `/` between its parts.
fn name_of(file: &Path, base: &Path) -> String {
    let absolute = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let file_path = absolute(file);
    let relative = file_path
        .strip_prefix(absolute(base))
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| file.to_path_buf());
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
