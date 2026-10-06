//! `explainsql top`: a database's costliest statements, from
//! pg_stat_statements. In a terminal, a list to pick one from: Enter shows
//! its plan, estimated and without running it (from PostgreSQL 16, with
//! `GENERIC_PLAN` for a statement with parameters; before, Enter tries
//! values for them), and p tries values for its parameters as `--params`
//! does. Elsewhere, or with --print, the list is printed.

use std::process::ExitCode;
use std::time::Duration;

use explainsql_core::report;
use explainsql_core::top::Entry;
use explainsql_db::{Database, Mode, Safety, Settings};
use explainsql_tui::{List, Picked};

use crate::{Format, Theme, TopArgs, connected, emit, interactive, page, params, use_color};

pub fn run(args: &TopArgs) -> ExitCode {
    match try_run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_run(args: &TopArgs) -> Result<ExitCode, String> {
    let settings = Settings::resolve(args.dbname.as_deref())?;
    let db = Database::connect(&settings).map_err(|error| error.to_string())?;
    let safety = Safety {
        allow_dml: args.allow_dml,
        allow_ddl: false,
        timeout: Duration::from_secs(args.timeout.max(1)),
    };
    let entries = db
        .statements(usize::from(args.limit), safety)
        .map_err(|error| error.to_string())?;
    let source = format!(
        "{}, PostgreSQL {}",
        db.description(),
        version(db.server_version())
    );
    if args.print || args.format != Format::Text || !interactive() {
        return Ok(emit(&match args.format {
            Format::Text => report::top_text(&entries, &source, use_color(args.color)),
            Format::Md => report::top_markdown(&entries, &source),
            Format::Json => report::top_json(&entries, &source),
        }));
    }

    let options = explainsql_tui::Options {
        background: match args.theme {
            Theme::Dark => explainsql_tui::Background::Dark,
            Theme::Light => explainsql_tui::Background::Light,
        },
        depth: None,
    };
    let mut list = List::new(entries, source, db.has_generic_plan());
    list.measure = args.measure;
    loop {
        let picked = explainsql_tui::pick(&mut list, options)
            .map_err(|error| format!("cannot open the list: {error}"))?;
        let (index, values) = match picked {
            Picked::Quit => return Ok(ExitCode::SUCCESS),
            Picked::Plan(index) => (index, false),
            Picked::Values(index) => (index, true),
        };
        let entry = &list.entries[index];
        let result = if values {
            eprintln!("explainsql: trying values for statement {}…", index + 1);
            try_values(&db, entry, args, safety)
        } else {
            eprintln!("explainsql: planning statement {}…", index + 1);
            show_plan(&db, entry, options, safety)
        };
        if let Err(error) = result {
            list.message = Some(error);
        }
    }
}

/// The plan of a statement, estimated: nothing runs. With parameters, the
/// generic plan, for any value.
fn show_plan(
    db: &Database,
    entry: &Entry,
    options: explainsql_tui::Options,
    safety: Safety,
) -> Result<(), String> {
    let json = if connected::placeholder_list(&entry.query).is_some() {
        db.generic_plan(&entry.query, safety)
    } else {
        db.explain(&entry.query, Mode::Estimate, safety)
    }
    .map_err(|error| error.to_string())?;
    let plan = explainsql_core::parse(&json).map_err(|error| error.to_string())?;
    let (analysis, _) = connected::analyzed(db, &plan);
    explainsql_tui::run(plan, analysis, options)
        .map_err(|error| format!("cannot open the viewer: {error}"))
}

/// How the statement's plan depends on its parameters' values, as
/// `--params` reports it, in the pager.
fn try_values(db: &Database, entry: &Entry, args: &TopArgs, safety: Safety) -> Result<(), String> {
    let trying = params::Trying {
        measure: args.measure,
        runs: 1,
        safety,
    };
    let (plan, sensitivity) = params::sensitivity(db, &entry.query, &[], trying)?;
    let (mut analysis, _) = connected::analyzed(db, &plan);
    analysis.parameters = Some(sensitivity);
    page(&report::text(&plan, &analysis, false));
    Ok(())
}

/// `server_version_num` as people write it: 160004 is 16.4, 90624 is 9.6.24.
fn version(number: u32) -> String {
    if number >= 100_000 {
        format!("{}.{}", number / 10_000, number % 10_000)
    } else {
        format!(
            "{}.{}.{}",
            number / 10_000,
            number / 100 % 100,
            number % 100
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_versions() {
        assert_eq!(super::version(160_004), "16.4");
        assert_eq!(super::version(180_000), "18.0");
        assert_eq!(super::version(90_624), "9.6.24");
    }
}
