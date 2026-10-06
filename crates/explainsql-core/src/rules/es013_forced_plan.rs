//! ES013: the plan was made with planner settings that take choices away,
//! such as `enable_seqscan = off`: set to test a plan and left on in the
//! session, or set for a role or a database. Applications that plan with
//! the defaults get another plan. A node the planner had to use anyway is
//! disabled: PostgreSQL 18 marks it, and before 18 its estimated cost
//! includes 10¹⁰ for each disabled node.
//!
//! Silent when the plan shows neither such settings nor a disabled node.

use super::{Context, Evidence, Finding, Rule, Severity, evidence};
use crate::compare::DISABLE_COST;
use crate::format;
use crate::ir::Node;

pub const RULE: Rule = Rule {
    id: "ES013",
    name: "Planner settings force the plan",
};

pub(super) fn check(context: &Context) -> Vec<Finding> {
    let settings: Vec<String> = context
        .plan
        .summary
        .settings
        .iter()
        .filter(|(name, value)| name.starts_with("enable_") && value.as_str() == "off")
        .map(|(name, value)| format!("{name} = {value}"))
        .collect();
    let disabled = disabled_nodes(context);
    let mut facts: Vec<Evidence> = Vec::new();
    if !settings.is_empty() {
        facts.push(evidence("Settings", settings.join(", ")));
    }
    if disabled.is_empty() {
        if settings.is_empty() {
            return Vec::new();
        }
        return vec![Finding {
            rule: RULE,
            severity: Severity::Medium,
            node: None,
            summary: format!(
                "The plan was made with {}, which {} the planner from some of its usual choices",
                list(&settings),
                if settings.len() == 1 { "keeps" } else { "keep" }
            ),
            evidence: facts,
            action: action(&settings),
        }];
    }
    disabled
        .into_iter()
        .map(|node| {
            let setting = setting_for(node)
                .map(|name| format!("{name} = off"))
                .filter(|setting| settings.is_empty() || settings.contains(setting));
            let mut evidence_list = facts.clone();
            evidence_list.push(evidence(
                "Disabled",
                if node.disabled {
                    "marked by PostgreSQL (Disabled: true)".to_owned()
                } else {
                    "its estimated cost includes 10¹⁰, which PostgreSQL adds for a disabled node"
                        .to_owned()
                },
            ));
            evidence_list.extend(context.time_evidence(node));
            let named = match &setting {
                Some(setting) => vec![setting.clone()],
                None => settings.clone(),
            };
            Finding {
                rule: RULE,
                severity: Severity::Medium,
                node: Some(node.id),
                summary: match &setting {
                    Some(setting) => format!(
                        "{} runs although {setting}: the planner found no other way",
                        format::node(node)
                    ),
                    None => format!(
                        "{} runs although a planner setting disables it: the planner found no other way",
                        format::node(node)
                    ),
                },
                evidence: evidence_list,
                action: if named.is_empty() {
                    "Applications that plan with the defaults may get another plan: find the planner settings that are off (SELECT name FROM pg_settings WHERE name LIKE 'enable%' AND setting = 'off'), reset them and run EXPLAIN again.".to_owned()
                } else {
                    action(&named)
                },
            }
        })
        .collect()
}

/// `a`, `a and b`, `a, b and c`.
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn action(settings: &[String]) -> String {
    let names: Vec<String> = settings
        .iter()
        .map(|setting| {
            setting
                .split(" = ")
                .next()
                .unwrap_or(setting.as_str())
                .to_owned()
        })
        .collect();
    format!(
        "Applications that plan with the defaults may get another plan: reset {} (RESET {}, or a new session) and run EXPLAIN again. If a role or a database sets {} (ALTER ROLE or ALTER DATABASE … SET), every session plans this way; to test a plan, SET LOCAL keeps a setting to one transaction.",
        if names.len() == 1 { "it" } else { "them" },
        names.join("; RESET "),
        if names.len() == 1 { "it" } else { "them" },
    )
}

/// The nodes the planner used although a setting disabled them: marked by
/// PostgreSQL 18, or before 18, those whose estimated startup cost holds
/// more of PostgreSQL's disable cost than their children's together. The
/// startup cost, unlike the total, does not repeat a nested loop's inner
/// side for each outer row.
fn disabled_nodes<'a>(context: &Context<'a>) -> Vec<&'a Node> {
    let marked: Vec<&Node> = context.nodes().filter(|node| node.disabled).collect();
    if !marked.is_empty() {
        return marked;
    }
    let disables = |node: &Node| {
        node.estimates.map_or(0.0, |estimates| {
            (estimates.startup_cost / DISABLE_COST).floor()
        })
    };
    context
        .nodes()
        .filter(|node| {
            let own = disables(node);
            let children: f64 = context.plan.children(node.id).map(disables).sum();
            own >= 1.0 && own > children
        })
        .collect()
}

/// The setting that disables a node's kind of operation.
fn setting_for(node: &Node) -> Option<&'static str> {
    Some(match node.node_type.as_str() {
        "Seq Scan" => "enable_seqscan",
        "Index Scan" => "enable_indexscan",
        "Index Only Scan" => "enable_indexonlyscan",
        "Bitmap Heap Scan" | "Bitmap Index Scan" => "enable_bitmapscan",
        "Tid Scan" | "Tid Range Scan" => "enable_tidscan",
        "Nested Loop" => "enable_nestloop",
        "Hash Join" => "enable_hashjoin",
        "Merge Join" => "enable_mergejoin",
        "Sort" => "enable_sort",
        "Incremental Sort" => "enable_incremental_sort",
        "Material" => "enable_material",
        "Memoize" => "enable_memoize",
        "Gather Merge" => "enable_gathermerge",
        "Aggregate" if matches!(node.strategy.as_deref(), Some("Hashed" | "Mixed")) => {
            "enable_hashagg"
        }
        "Append" if node.parallel_aware => "enable_parallel_append",
        "Hash" if node.parallel_aware => "enable_parallel_hash",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use crate::ir::Plan;
    use crate::metrics;
    use crate::rules::{self, Finding};

    fn findings(text: &str) -> Vec<Finding> {
        let plan: Plan = crate::parse(text).unwrap();
        rules::check(&plan, &metrics::compute(&plan))
            .into_iter()
            .filter(|finding| finding.rule.id == "ES013")
            .collect()
    }

    #[test]
    fn names_the_node_postgresql_had_to_use_anyway() {
        // PostgreSQL 18 marks the node.
        let marked = findings(
            "\
Seq Scan on audit_log  (cost=0.00..94.50 rows=1250 width=18) (actual time=0.015..0.260 rows=1250.00 loops=1)
  Disabled: true
  Filter: (action = 'login'::text)
  Rows Removed by Filter: 3750
Settings: enable_seqscan = 'off'
Execution Time: 0.339 ms",
        );
        assert_eq!(marked.len(), 1);
        assert_eq!(
            marked[0].summary,
            "Seq Scan on audit_log runs although enable_seqscan = off: the planner found no other way"
        );
        assert!(
            marked[0].action.contains("RESET enable_seqscan"),
            "{}",
            marked[0].action
        );
        // Before 18: the disable cost, in the scan, not in the sort above it.
        let costed = findings(
            "\
Sort  (cost=10000000150.00..10000000153.00 rows=1250 width=18)
  Sort Key: at
  ->  Seq Scan on audit_log  (cost=10000000000.00..10000000094.50 rows=1250 width=18)
        Filter: (action = 'login'::text)",
        );
        assert_eq!(costed.len(), 1);
        assert_eq!(costed[0].node, Some(crate::ir::NodeId(1)));
        assert!(
            costed[0]
                .evidence
                .iter()
                .any(|fact| fact.value.contains("10¹⁰"))
        );
    }

    #[test]
    fn names_the_settings_when_no_node_is_disabled() {
        let forced = findings(
            "\
Merge Join  (cost=1.00..10.00 rows=10 width=8)
  Merge Cond: (o.customer_id = c.id)
  ->  Index Scan using orders_customer_id_idx on orders o  (cost=0.29..5.00 rows=10 width=8)
  ->  Index Scan using customers_pkey on customers c  (cost=0.29..5.00 rows=10 width=4)
Settings: enable_hashjoin = 'off', enable_nestloop = 'off', max_parallel_workers_per_gather = '0'",
        );
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].node, None);
        assert_eq!(
            forced[0].summary,
            "The plan was made with enable_hashjoin = off and enable_nestloop = off, which keep the planner from some of its usual choices"
        );
        assert!(
            forced[0]
                .action
                .contains("RESET enable_hashjoin; RESET enable_nestloop")
        );
        // Other settings are tuning, not forcing.
        assert!(
            findings(
                "Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)\nSettings: work_mem = '64MB', enable_partitionwise_join = 'on'"
            )
            .is_empty()
        );
    }
}
