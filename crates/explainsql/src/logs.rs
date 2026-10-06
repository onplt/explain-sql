//! `explainsql logs`: auto_explain plans from server logs, and for each
//! statement, whether, when and how its plan changed.

use std::process::ExitCode;

use explainsql_core::pg::LogEntry;
use explainsql_core::report;
use explainsql_core::timeline::{self, Pattern, Timeline};

use crate::{Format, LogsArgs, emit, read_input, use_color};

pub fn run(args: &LogsArgs) -> ExitCode {
    match try_run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn try_run(args: &LogsArgs) -> Result<ExitCode, String> {
    let mut entries: Vec<LogEntry> = Vec::new();
    for file in &args.files {
        let text = read_input(Some(file), false).map_err(|error| error.to_string())?;
        let (mut found, skipped) =
            explainsql_core::parse_log(&text).map_err(|error| format!("{file}: {error}"))?;
        for warning in skipped {
            eprintln!(
                "explainsql: {file}:{}: {}",
                warning.line.unwrap_or(0),
                warning.message
            );
        }
        if args.files.len() > 1 {
            for entry in &mut found {
                entry.meta.file = Some(file.clone());
            }
        }
        entries.extend(found);
    }

    // Times, as the log prints them.
    let times: Vec<Option<i64>> = entries
        .iter()
        .map(|entry| timeline::instant(entry.meta.timestamp.as_deref()))
        .collect();
    let newest = times.iter().flatten().copied().max();
    let since = args
        .since
        .as_deref()
        .map(|text| bound(text, newest))
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(|text| bound(text, newest))
        .transpose()?;
    if since.is_some() || until.is_some() {
        let keep: Vec<bool> = times
            .iter()
            .map(|time| {
                time.is_some_and(|time| {
                    since.is_none_or(|since| time >= since)
                        && until.is_none_or(|until| time <= until)
                })
            })
            .collect();
        let mut keep = keep.into_iter();
        entries.retain(|_| keep.next().unwrap_or(false));
    }
    if entries.is_empty() {
        return Err("no auto_explain entry in that time".to_owned());
    }

    // The statements asked for, each with all its entries.
    let all = timeline::timeline(&entries);
    let wanted: Vec<usize> = all
        .statements
        .iter()
        .filter(|statement| {
            args.query
                .as_deref()
                .is_none_or(|query| match query.parse::<i64>() {
                    Ok(id) => statement.query_id == Some(id),
                    Err(_) => {
                        // Its text, the name it was prepared under, or its tags.
                        let query = query.to_lowercase();
                        std::iter::once(&statement.text)
                            .chain(statement.prepared.as_ref())
                            .chain(statement.tags.values())
                            .any(|text| text.to_lowercase().contains(&query))
                    }
                })
        })
        .filter(|statement| {
            args.trace.as_deref().is_none_or(|trace| {
                statement
                    .entries
                    .iter()
                    .any(|&index| in_trace(&entries[index], trace))
            })
        })
        .filter(|statement| !args.changed || statement.pattern != Pattern::Stable)
        .flat_map(|statement| statement.entries.iter().copied())
        .collect();
    let timeline: Timeline = if wanted.len() == entries.len() {
        all
    } else {
        let mut wanted = wanted;
        wanted.sort_unstable();
        entries = wanted
            .into_iter()
            .map(|index| entries[index].clone())
            .collect();
        timeline::timeline(&entries)
    };
    if timeline.statements.is_empty() {
        eprintln!("explainsql: no statement matches");
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = String::new();
    // Which plans the trace's statements ran.
    if let (Some(trace), Format::Text | Format::Md) = (args.trace.as_deref(), args.format) {
        for statement in &timeline.statements {
            for &index in &statement.entries {
                let entry = &entries[index];
                if !in_trace(entry, trace) {
                    continue;
                }
                let shape = explainsql_core::fingerprint::id(&entry.plan);
                let plan = statement
                    .plans
                    .iter()
                    .position(|plan| plan.shape == shape)
                    .map_or(0, |number| number + 1);
                out.push_str(&format!(
                    "Trace {trace}: {} at {}, line {}, ran plan {plan} of {}{}.\n",
                    statement.text,
                    entry.meta.timestamp.as_deref().unwrap_or("an unknown time"),
                    entry.meta.line,
                    statement.plans.len(),
                    entry
                        .meta
                        .duration()
                        .map(|ms| format!(", {}", explainsql_core::format::duration(ms)))
                        .unwrap_or_default()
                ));
            }
        }
        out.push('\n');
    }
    out.push_str(&match args.format {
        Format::Text => report::logs_text(&entries, &timeline, use_color(args.color)),
        Format::Md => report::logs_markdown(&entries, &timeline),
        Format::Json => report::logs_json(&entries, &timeline),
    });
    Ok(emit(&out))
}

/// Whether an entry ran in a trace: its sqlcommenter traceparent names it.
fn in_trace(entry: &LogEntry, trace: &str) -> bool {
    entry
        .plan
        .summary
        .query_text
        .as_deref()
        .map(timeline::tags)
        .and_then(|tags| tags.get("traceparent").cloned())
        .is_some_and(|parent| {
            timeline::trace_id(&parent).is_some_and(|id| id.eq_ignore_ascii_case(trace))
                || parent.contains(trace)
        })
}

/// A time to filter by: as the log prints times, or `30m`, `24h`, `7d`
/// back from the newest entry.
fn bound(text: &str, newest: Option<i64>) -> Result<i64, String> {
    let text = text.trim();
    let unit = match text.chars().last() {
        Some('s') => Some(1_000),
        Some('m') => Some(60_000),
        Some('h') => Some(3_600_000),
        Some('d') => Some(86_400_000),
        _ => None,
    };
    if let (Some(unit), Ok(count)) = (unit, text[..text.len() - 1].parse::<i64>()) {
        let newest = newest.ok_or("the log's entries have no times to count back from")?;
        return Ok(newest - count * unit);
    }
    timeline::instant(Some(text)).ok_or_else(|| {
        format!("{text}: give a time as the log prints it (2026-10-06 06:00) or a span back from its last entry (30m, 24h, 7d)")
    })
}
