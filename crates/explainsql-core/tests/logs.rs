//! Server logs with auto_explain entries, captured from a real session in
//! the three formats PostgreSQL writes (`fixtures/logs/`).

mod common;

use explainsql_core::parse_log;
use explainsql_core::timeline::{Pattern, timeline};

fn log(name: &str) -> String {
    common::read(&common::fixtures().join("logs").join(name))
}

#[test]
fn reads_every_entry_with_what_the_log_says() {
    for name in ["postgresql.log", "postgresql.csv", "postgresql.json"] {
        let (entries, skipped) = parse_log(&log(name)).unwrap();
        assert!(skipped.is_empty(), "{name}: {skipped:?}");
        // Two runs of the session: text plans, then JSON plans.
        assert_eq!(entries.len(), 32, "{name}");
        let first = &entries[0];
        assert!(
            first
                .meta
                .timestamp
                .as_deref()
                .is_some_and(|stamp| stamp.starts_with("2026-10-06 06:35:13")),
            "{name}: {:?}",
            first.meta
        );
        assert!(first.meta.pid.is_some(), "{name}");
        assert_eq!(first.meta.user.as_deref(), Some("postgres"), "{name}");
        assert_eq!(first.meta.database.as_deref(), Some("shop"), "{name}");
        assert!(first.meta.duration().is_some(), "{name}");
        let query = first.plan.summary.query_text.as_deref().unwrap();
        assert!(
            query.starts_with("SELECT id, status, amount FROM orders"),
            "{name}: {query}"
        );
        assert!(query.contains("traceparent="), "{name}");
        assert!(first.plan.summary.query_identifier.is_some(), "{name}");
        // The prepared statement's entries carry its parameters.
        let executed: Vec<_> = entries
            .iter()
            .filter(|entry| entry.parameters.is_some())
            .collect();
        assert_eq!(executed.len(), 16, "{name}");
        assert!(
            executed[0]
                .plan
                .summary
                .query_text
                .as_deref()
                .unwrap()
                .starts_with("PREPARE latest"),
            "{name}"
        );
        assert_eq!(
            executed[0].parameters.as_deref(),
            Some("$1 = '4242', $2 = '10'")
        );
        // Text plans first, JSON plans in the second run.
        assert_eq!(
            entries[0].plan.source.format,
            explainsql_core::ir::Format::Text
        );
        assert_eq!(
            entries[16].plan.source.format,
            explainsql_core::ir::Format::Json
        );
    }
    // The application name is in jsonlog and csvlog records; the default
    // stderr prefix leaves it out.
    let (json, _) = parse_log(&log("postgresql.json")).unwrap();
    assert_eq!(json[0].meta.application.as_deref(), Some("shop-api"));
    let (csv, _) = parse_log(&log("postgresql.csv")).unwrap();
    assert_eq!(csv[3].meta.application.as_deref(), Some("shop-reports"));
}

#[test]
fn tells_when_each_plan_changed() {
    for name in ["postgresql.log", "postgresql.csv", "postgresql.json"] {
        let (entries, _) = parse_log(&log(name)).unwrap();
        let timeline = timeline(&entries);
        assert_eq!(timeline.statements.len(), 3, "{name}");
        // The prepared statement first: its generic plan cost the most.
        let prepared = &timeline.statements[0];
        assert_eq!(prepared.prepared.as_deref(), Some("latest"), "{name}");
        assert_eq!(prepared.runs, 16);
        assert_eq!(prepared.pattern, Pattern::Changed);
        assert_eq!(prepared.plans.len(), 2);
        assert!(prepared.plans[1].generic && !prepared.plans[0].generic);
        let generic: Vec<bool> = prepared
            .changes
            .iter()
            .map(|change| change.generic)
            .collect();
        assert_eq!(generic, [true, false, true], "{name}");
        let first = &prepared.changes[0];
        // The sixth execution, after five custom plans.
        assert_eq!(first.runs_before, 5);
        assert_eq!(first.parameters.as_deref(), Some("$1 = '777', $2 = '10'"));
        assert!(first.median_after.unwrap() > 3.0 * first.median_before.unwrap());
        assert!(
            first.verdict.starts_with("Worse: pages"),
            "{}",
            first.verdict
        );
        // The next run of the session prepares the statement again.
        assert!(prepared.changes[1].new_session);

        // The latest orders: the index served them until it was dropped.
        let latest = &timeline.statements[1];
        assert_eq!(latest.runs, 12);
        assert_eq!(latest.tags["controller"], "OrderController");
        assert_eq!(latest.tags["action"], "latest");
        assert!(!latest.tags.contains_key("traceparent"));
        assert!(latest.plans[0].access.contains("orders_customer_id_idx"));
        assert_eq!(latest.plans[1].access, "Seq Scan on orders");
        assert_eq!(latest.changes.len(), 3);

        // The daily report kept its plan.
        let daily = &timeline.statements[2];
        assert_eq!(daily.pattern, Pattern::Stable);
        assert_eq!(daily.runs, 4);
        assert!(daily.changes.is_empty());
    }
}
