//! The index advisor against every plan in the corpus. Each scenario's
//! header says what the advisor must conclude: `advice: none` (no
//! suggestion: a trap for naive advisors), `advice: rewrite` (fix the query)
//! or `advice: index` with the exact indexes in `index:` lines. A scenario
//! without an `advice` header must get no suggestion either, so that every
//! suggestion the corpus produces has been reviewed.

mod common;

use std::collections::BTreeSet;

use common::{corpus, fixtures, plan_path, read};
use explainsql_core::advisor::AdviceKind;
use explainsql_core::{analyze, parse};

struct Expected {
    advice: Option<String>,
    indexes: Vec<String>,
    optional: Vec<String>,
}

fn expected(scenario: &str) -> Expected {
    let source = read(&fixtures().join("scenarios").join(format!("{scenario}.sql")));
    let mut expected = Expected {
        advice: None,
        indexes: Vec::new(),
        optional: Vec::new(),
    };
    for line in source.lines().take_while(|line| line.starts_with("--")) {
        if let Some(value) = line.strip_prefix("-- advice:") {
            expected.advice = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("-- index:") {
            let value = value.trim();
            match value.strip_suffix('?') {
                Some(index) => expected.optional.push(index.to_owned()),
                None => expected.indexes.push(value.to_owned()),
            }
        }
    }
    expected
}

#[test]
fn every_plan_gets_exactly_its_advice() {
    let mut problems = Vec::new();
    let (mut traps, mut suggestions) = (0, 0);
    for (major, scenario) in corpus() {
        let expected = expected(&scenario);
        for extension in ["json", "txt"] {
            let label = format!("PostgreSQL {major} {scenario}.{extension}");
            let plan = parse(&read(&plan_path(major, &scenario, extension))).unwrap();
            let analysis = analyze(&plan);
            let mut targets = BTreeSet::new();
            let mut rewrites = 0;
            for advice in &analysis.advice {
                match &advice.kind {
                    AdviceKind::Index { index, ddl } => {
                        assert!(ddl.starts_with("CREATE INDEX"), "{label}: {ddl}");
                        targets.insert(index.target());
                    }
                    AdviceKind::ForeignKey { constraint } => {
                        targets.insert(format!("constraint {constraint}"));
                    }
                    AdviceKind::Rewrite { .. } => rewrites += 1,
                    AdviceKind::NoIndex { .. } | AdviceKind::AlreadyIndexed { .. } => {}
                }
            }
            suggestions += targets.len();
            match expected.advice.as_deref() {
                Some("index") => {
                    for index in &expected.indexes {
                        if !targets.contains(index) {
                            problems.push(format!("{label}: missing {index}"));
                        }
                    }
                    for target in &targets {
                        if !expected.indexes.contains(target) && !expected.optional.contains(target)
                        {
                            problems.push(format!("{label}: unexpected {target}"));
                        }
                    }
                }
                Some("rewrite") => {
                    if rewrites == 0 {
                        problems.push(format!("{label}: no rewrite suggested"));
                    }
                    if !targets.is_empty() {
                        problems.push(format!("{label}: unexpected {targets:?}"));
                    }
                }
                Some("none") | None => {
                    if expected.advice.is_some() {
                        traps += 1;
                    }
                    if !targets.is_empty() || rewrites > 0 {
                        problems.push(format!(
                            "{label}: expected no suggestion, got {targets:?} and {rewrites} rewrites"
                        ));
                    }
                }
                Some(other) => problems.push(format!("{label}: unknown advice `{other}`")),
            }
        }
    }
    assert!(traps > 50, "only {traps} trap plans");
    assert!(suggestions > 100, "only {suggestions} suggestions");
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// What the advice says, on a few representative plans.
#[test]
fn advice_explains_itself() {
    let advice = |scenario: &str| {
        let plan = parse(&read(&plan_path(16, scenario, "txt"))).unwrap();
        analyze(&plan).advice
    };
    let selective = advice("seq_scan_selective");
    let AdviceKind::Index { ddl, .. } = &selective[0].kind else {
        panic!("{selective:?}");
    };
    assert_eq!(
        ddl,
        "CREATE INDEX CONCURRENTLY ON public.orders (customer_id);"
    );
    assert!(selective[0].caveats[0].starts_with("Not connected"));

    let partitions = advice("seq_scan_jsonb_containment");
    let AdviceKind::Index { ddl, .. } = &partitions[0].kind else {
        panic!("{partitions:?}");
    };
    // No CONCURRENTLY on a partitioned table.
    assert_eq!(ddl, "CREATE INDEX ON public.events USING gin (payload);");

    let lateral = advice("lateral_join_top_n");
    assert!(
        lateral[0]
            .caveats
            .iter()
            .any(|caveat| caveat.contains("orders_created_at_idx")),
        "{:?}",
        lateral[0].caveats
    );

    let or = advice("seq_scan_or_across_columns");
    assert!(
        matches!(&or[0].kind, AdviceKind::NoIndex { reason } if reason.contains("BitmapOr")),
        "{or:?}"
    );
    let tiny = advice("seq_scan_tiny_table");
    assert!(
        matches!(&tiny[0].kind, AdviceKind::NoIndex { reason } if reason.contains("small")),
        "{tiny:?}"
    );
}
