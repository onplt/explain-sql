//! Where candidates come from: the findings of the rules about scans, three
//! patterns no rule covers, and explanations for the slow scans left over.

use super::keys::Keys;
use super::{Advice, AdviceKind, Confidence, Index, NOT_CONNECTED, Verification, kept_evidence};
use crate::format;
use crate::ir::{Node, NodeId, Plan, PredicateKind, Relationship};
use crate::metrics::{self, Metrics};
use crate::rules::{Evidence, Finding, Severity, qualifier, scan_below};

/// Share of the runtime from which a pattern is worth an index.
const MIN_SHARE: f64 = 0.1;
/// Rows kept per row read, below which a scan is selective.
const MAX_SELECTIVITY: f64 = 0.05;
/// Pages from which a table is large.
const MIN_PAGES: f64 = 1000.0;
/// Rows read from which a table is large, when there are no buffer counts.
const MIN_ROWS: f64 = 50_000.0;

pub struct Context<'a> {
    pub plan: &'a Plan,
    pub metrics: &'a Metrics,
}

impl Context<'_> {
    /// The node's own share of the runtime, by time or else by buffers.
    fn share(&self, node: &Node) -> Option<f64> {
        let metrics = self.metrics.node(node.id);
        metrics.time_share.or_else(|| {
            let total = metrics::blocks(&self.plan.root().buffers?);
            let own = metrics::blocks(&metrics.exclusive_buffers?);
            (total > 0).then(|| own as f64 / total as f64)
        })
    }

    /// The share of the runtime spent in the node and below it.
    fn inclusive_share(&self, node: &Node) -> Option<f64> {
        let total = self.metrics.statement.total_time?;
        self.metrics
            .node(node.id)
            .inclusive_time
            .map(|time| time / total)
            .or_else(|| {
                let total = metrics::blocks(&self.plan.root().buffers?);
                let own = metrics::blocks(&node.buffers?);
                (total > 0).then(|| own as f64 / total as f64)
            })
    }

    fn parent(&self, node: &Node) -> Option<&Node> {
        node.parent.map(|id| self.plan.node(id))
    }

    /// Pages a scan reads each time it runs, when the plan counts buffers.
    fn pages_per_scan(&self, node: &Node) -> Option<f64> {
        let actuals = node.actuals?;
        let scans = actuals.loops as f64 / self.metrics.node(node.id).processes;
        node.buffers
            .map(|buffers| metrics::blocks(&buffers) as f64 / scans.max(1.0))
    }
}

/// An index advice from keys found on a scan.
fn index_advice(
    scan: &Node,
    index: Index,
    keys: &Keys,
    severity: Option<Severity>,
    summary: String,
    evidence: Vec<Evidence>,
    rules: Vec<&'static str>,
) -> Advice {
    let mut confidence = match severity {
        Some(Severity::High) | None => Confidence::High,
        Some(_) => Confidence::Medium,
    };
    let mut caveats = vec![NOT_CONNECTED.to_owned()];
    if keys.needs_pattern_ops {
        confidence = confidence.min(Confidence::Medium);
        caveats.push(
            "text_pattern_ops is needed unless the column uses the C collation; with C, a plain index serves LIKE prefix searches.".to_owned(),
        );
    }
    if keys.needs_trigram {
        confidence = confidence.min(Confidence::Medium);
        caveats.push(
            "gin_trgm_ops comes with the pg_trgm extension: CREATE EXTENSION IF NOT EXISTS pg_trgm."
                .to_owned(),
        );
    }
    if keys.runtime_value {
        confidence = confidence.min(Confidence::Medium);
        caveats.push(
            "The condition compares with a value known only at run time, so whether the planner uses the index depends on that value.".to_owned(),
        );
    }
    let ddl = index.ddl();
    Advice {
        node: Some(scan.id),
        kind: AdviceKind::Index { index, ddl },
        confidence,
        summary,
        evidence,
        caveats,
        verification: Verification::Unverified,
        rules,
    }
}

/// The schema and table a scan reads.
fn table(scan: &Node) -> Option<(Option<String>, String)> {
    Some((scan.schema.clone(), scan.relation_name.clone()?))
}

/// Advice for the findings that are about scans.
pub fn from_finding(context: &Context, finding: &Finding) -> Option<Advice> {
    match finding.rule.id {
        "ES001" => selective_scan(context, finding),
        "ES005" => nested_loop(context, finding),
        "ES006" => filtering_index_scan(context, finding),
        "ES009" => foreign_key(finding),
        _ => None,
    }
}

/// ES001: index the filtered columns, or rewrite a condition that wraps
/// them.
fn selective_scan(context: &Context, finding: &Finding) -> Option<Advice> {
    let scan = context.plan.node(finding.node?);
    let own = qualifier(scan);
    let mut keys = Keys::default();
    let join = context
        .parent(scan)
        .filter(|parent| {
            parent.node_type == "Nested Loop" && scan.relationship == Some(Relationship::Inner)
        })
        .and_then(|parent| parent.predicate(PredicateKind::JoinFilter));
    if let Some(condition) = join {
        keys.add(condition, own, true);
    }
    if let Some(condition) = scan.predicate(PredicateKind::Filter) {
        keys.add(condition, own, join.is_some());
    }
    let (schema, name) = table(scan)?;
    if let Some(index) = keys.index(schema, name) {
        let summary = if join.is_some() {
            format!(
                "An index on {} would turn each loop of {} into an index lookup instead of a scan of the whole table.",
                index.target(),
                format::node(scan)
            )
        } else {
            format!(
                "An index on {} would let {} read only the rows it keeps instead of the whole table.",
                index.target(),
                format::node(scan)
            )
        };
        return Some(index_advice(
            scan,
            index,
            &keys,
            Some(finding.severity),
            summary,
            finding.evidence.clone(),
            vec![finding.rule.id],
        ));
    }
    let (column, wrapper) = keys.wrapped.clone()?;
    let shown = if wrapper == "a cast" {
        "a cast".to_owned()
    } else {
        format!("{wrapper}()")
    };
    Some(Advice {
        node: Some(scan.id),
        summary: format!(
            "The filter applies {shown} to {column}, so no index on {column} can serve it. Compare {column} itself with a value of its own type, or a range of values, instead."
        ),
        kind: AdviceKind::Rewrite { column, wrapper },
        confidence: Confidence::High,
        evidence: finding.evidence.clone(),
        caveats: vec![
            "An index on the expression itself would also work, but only for an immutable expression written exactly as in the query.".to_owned(),
        ],
        verification: Verification::Unverified,
        rules: vec![finding.rule.id],
    })
}

/// ES005: index the inner side's join key.
fn nested_loop(context: &Context, finding: &Finding) -> Option<Advice> {
    let join = context.plan.node(finding.node?);
    let inner = context
        .plan
        .children(join.id)
        .find(|child| child.relationship == Some(Relationship::Inner))?;
    let scan = scan_below(context.plan, inner)?;
    let own = qualifier(scan);
    let mut keys = Keys::default();
    if let Some(condition) = join.predicate(PredicateKind::JoinFilter) {
        keys.add(condition, own, true);
    }
    add_scan_conditions(context, scan, &mut keys);
    let (schema, name) = table(scan)?;
    let index = keys.index(schema, name)?;
    let loops = inner.actuals.map_or(0.0, |actuals| actuals.loops as f64);
    let summary = format!(
        "An index on {} would turn each of the {} runs of {} into an index lookup.",
        index.target(),
        format::rows(loops),
        format::node(scan)
    );
    let mut advice = index_advice(
        scan,
        index,
        &keys,
        Some(finding.severity),
        summary,
        finding.evidence.clone(),
        vec![finding.rule.id],
    );
    inferred_sort_caveat(context, scan, &mut advice);
    Some(advice)
}

/// ES006: a composite index that also covers the filtered columns.
fn filtering_index_scan(context: &Context, finding: &Finding) -> Option<Advice> {
    let scan = context.plan.node(finding.node?);
    let mut keys = Keys::default();
    add_scan_conditions(context, scan, &mut keys);
    let (schema, name) = table(scan)?;
    let index = keys.index(schema, name)?;
    let summary = format!(
        "An index on {} would let the index do the filtering that {} now does on every row it finds.",
        index.target(),
        format::node(scan)
    );
    let mut advice = index_advice(
        scan,
        index,
        &keys,
        Some(finding.severity),
        summary,
        finding.evidence.clone(),
        vec![finding.rule.id],
    );
    inferred_sort_caveat(context, scan, &mut advice);
    Some(advice)
}

/// A scan's own conditions, the index condition it already uses, and the
/// order it was asked for.
fn add_scan_conditions(context: &Context, scan: &Node, keys: &mut Keys) {
    let own = qualifier(scan);
    if let Some(condition) = scan.predicate(PredicateKind::Filter) {
        keys.add(condition, own, true);
    }
    let index_node = if scan.node_type == "Bitmap Heap Scan" {
        context
            .plan
            .children(scan.id)
            .find(|child| child.index_name.is_some())
            .unwrap_or(scan)
    } else {
        scan
    };
    let index_condition = index_node
        .predicate(PredicateKind::IndexCond)
        .or(scan.predicate(PredicateKind::RecheckCond));
    if let Some(condition) = index_condition {
        keys.add(condition, own, true);
    } else if let Some((column, descending)) = index_order(context, scan) {
        keys.sort(column, descending);
    }
}

/// The column an index scan reads in order of, for an index scan with no
/// condition under a `Limit`: it was chosen for its order. Plans do not name
/// index columns, so this relies on PostgreSQL's default index name,
/// `<table>_<column>_idx`, and the column appearing in the output.
fn index_order(context: &Context, scan: &Node) -> Option<(String, bool)> {
    if !matches!(scan.node_type.as_str(), "Index Scan" | "Index Only Scan") {
        return None;
    }
    let under_limit = context
        .parent(scan)
        .is_some_and(|parent| parent.node_type == "Limit");
    if !under_limit {
        return None;
    }
    let index = scan.index_name.as_deref()?;
    let relation = scan.relation_name.as_deref()?;
    let column = index
        .strip_prefix(relation)?
        .strip_prefix('_')?
        .strip_suffix("_idx")?;
    let in_output = scan
        .output
        .iter()
        .any(|output| crate::expr::split_column(output).1 == column);
    if column.is_empty() || !in_output {
        return None;
    }
    let descending = scan.scan_direction.as_deref() == Some("Backward");
    Some((column.to_owned(), descending))
}

/// When the sort column came from an index name, says so.
fn inferred_sort_caveat(context: &Context, scan: &Node, advice: &mut Advice) {
    let has_condition = scan.predicate(PredicateKind::IndexCond).is_some();
    if has_condition {
        return;
    }
    if let (Some((column, _)), Some(index)) = (index_order(context, scan), &scan.index_name) {
        advice.confidence = advice.confidence.min(Confidence::Medium);
        advice.caveats.push(format!(
            "{column} is taken from the name of {index}, which {} reads in order; plans do not list index columns.",
            format::node(scan)
        ));
    }
}

/// ES009: the referencing columns of a foreign key. The plan names only
/// the constraint.
fn foreign_key(finding: &Finding) -> Option<Advice> {
    let constraint = finding
        .evidence
        .iter()
        .find(|evidence| evidence.label == "Constraint")?
        .value
        .clone();
    Some(Advice {
        node: None,
        summary: format!(
            "Each row deleted or updated makes PostgreSQL look up the rows that reference it through {constraint}. Without an index on the referencing columns, every lookup scans the referencing table."
        ),
        caveats: vec![
            format!(
                "The plan does not say which columns {constraint} covers. This query names them: SELECT conrelid::regclass, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{constraint}';"
            ),
            NOT_CONNECTED.to_owned(),
        ],
        kind: AdviceKind::ForeignKey { constraint },
        confidence: Confidence::High,
        evidence: finding.evidence.clone(),
        verification: Verification::Unverified,
        rules: vec![finding.rule.id],
    })
}

/// `ORDER BY … LIMIT` sorting a whole large table to return a few rows: an
/// index in the sort order returns them directly.
pub fn top_n(context: &Context) -> Vec<Advice> {
    let mut advice = Vec::new();
    for limit in &context.plan.nodes {
        if limit.node_type != "Limit" {
            continue;
        }
        let Some(sort) = context.plan.children(limit.id).find(|child| {
            child.node_type == "Sort"
                && child
                    .extra_str("Sort Method")
                    .is_some_and(|method| method.starts_with("top-N"))
        }) else {
            continue;
        };
        let Some(input) = context.plan.children(sort.id).next() else {
            continue;
        };
        let Some(scan) =
            scan_below(context.plan, input).filter(|scan| scan.node_type == "Seq Scan")
        else {
            continue;
        };
        let (Some(actuals), Some(limit_actuals)) = (scan.actuals, limit.actuals) else {
            continue;
        };
        let read = (actuals.rows + scan.rows_removed_by_filter) * actuals.loops as f64;
        let large = match context.pages_per_scan(scan) {
            Some(pages) => pages >= MIN_PAGES,
            None => read >= MIN_ROWS,
        };
        let hot = context
            .inclusive_share(sort)
            .is_some_and(|share| share >= MIN_SHARE);
        // Only a few rows of many are wanted.
        let few = limit_actuals.rows * 10.0 <= actuals.rows;
        if !large || !hot || !few || actuals.loops != 1 {
            continue;
        }
        let own = qualifier(scan);
        let mut keys = Keys::default();
        if let Some(condition) = scan.predicate(PredicateKind::Filter) {
            keys.add(condition, own, false);
            if keys.or_across_columns || keys.wrapped.is_some() {
                continue;
            }
        }
        // Every sort key must be a plain column of the scanned table.
        let mut plain = true;
        for key in &sort.sort_key {
            let (column, descending) = match key.strip_suffix(" DESC") {
                Some(column) => (column, true),
                None => (key.strip_suffix(" ASC").unwrap_or(key), false),
            };
            match crate::expr::operand(column) {
                crate::expr::Operand::Column(column)
                    if crate::expr::split_column(column)
                        .0
                        .is_none_or(|q| Some(q) == own) =>
                {
                    keys.sort(crate::expr::split_column(column).1.to_owned(), descending);
                }
                _ => plain = false,
            }
        }
        if !plain || sort.sort_key.is_empty() {
            continue;
        }
        let Some((schema, name)) = table(scan) else {
            continue;
        };
        let Some(index) = keys.index(schema, name) else {
            continue;
        };
        let summary = format!(
            "An index on {} would return the first {} rows in order, instead of reading and sorting all {}.",
            index.target(),
            format::rows(limit_actuals.rows),
            format::rows(read)
        );
        let mut evidence = vec![
            Evidence {
                label: "Sort",
                value: format!(
                    "{} by {}",
                    sort.extra_str("Sort Method").unwrap_or("top-N heapsort"),
                    sort.sort_key.join(", ")
                ),
            },
            Evidence {
                label: "Rows read to return",
                value: format!(
                    "{} of {}",
                    format::rows(limit_actuals.rows),
                    format::rows(read)
                ),
            },
        ];
        if let Some(share) = context.inclusive_share(sort) {
            evidence.push(Evidence {
                label: "Scan and sort",
                value: format!("{} of the runtime", format::percent(share)),
            });
        }
        advice.push(index_advice(
            scan,
            index,
            &keys,
            None,
            summary,
            evidence,
            Vec::new(),
        ));
    }
    advice
}

/// Selective scans of every partition of a partitioned table, each too small
/// for ES001 but large together: one index on the partitioned table.
pub fn partitions(context: &Context, existing: &[Advice]) -> Vec<Advice> {
    let mut advice = Vec::new();
    for append in &context.plan.nodes {
        if !matches!(append.node_type.as_str(), "Append" | "Merge Append") {
            continue;
        }
        let scans: Vec<&Node> = context
            .plan
            .children(append.id)
            .filter(|child| {
                !matches!(
                    child.relationship,
                    Some(Relationship::InitPlan | Relationship::SubPlan)
                )
            })
            .collect();
        let all_filtered_scans = scans.len() >= 2
            && scans.iter().all(|scan| {
                scan.node_type == "Seq Scan"
                    && scan.relation_name.is_some()
                    && scan.predicate(PredicateKind::Filter).is_some()
                    && !context.metrics.node(scan.id).may_stop_early
            });
        if !all_filtered_scans
            || scans
                .iter()
                .any(|scan| existing.iter().any(|advice| advice.node == Some(scan.id)))
        {
            continue;
        }
        let (mut read, mut kept, mut pages, mut share) = (0.0, 0.0, 0.0, 0.0);
        let mut counted_pages = true;
        for scan in &scans {
            let Some(actuals) = scan.actuals.filter(|actuals| !actuals.never_executed()) else {
                continue;
            };
            let loops = actuals.loops as f64;
            read += (actuals.rows + scan.rows_removed_by_filter) * loops;
            kept += actuals.rows * loops;
            match scan.buffers {
                Some(buffers) => pages += metrics::blocks(&buffers) as f64,
                None => counted_pages = false,
            }
            share += context.share(scan).unwrap_or(0.0);
        }
        let large = if counted_pages {
            pages >= MIN_PAGES
        } else {
            read >= MIN_ROWS
        };
        if read <= 0.0 || kept / read >= MAX_SELECTIVITY || !large || share < MIN_SHARE {
            continue;
        }
        let Some(parent) = parent_table(&scans) else {
            continue;
        };
        // The same filter on every partition, but for the alias.
        let first = scans[0];
        let mut keys = Keys::default();
        keys.add(
            first.predicate(PredicateKind::Filter).unwrap_or_default(),
            qualifier(first),
            false,
        );
        let Some(mut index) = keys.index(first.schema.clone(), parent.clone()) else {
            continue;
        };
        index.partitioned = true;
        let summary = format!(
            "An index on {} would let each of the {} partition scans read only the rows it keeps.",
            index.target(),
            scans.len()
        );
        let mut evidence = vec![
            Evidence {
                label: "Partitions scanned",
                value: format::rows(scans.len() as f64),
            },
            kept_evidence(kept, read),
        ];
        if counted_pages {
            evidence.push(Evidence {
                label: "Pages read",
                value: format::pages(pages),
            });
        }
        evidence.push(Evidence {
            label: "Time in the scans",
            value: format!("{} of the runtime", format::percent(share)),
        });
        let mut item = index_advice(first, index, &keys, None, summary, evidence, Vec::new());
        item.node = Some(append.id);
        item.confidence = item.confidence.min(Confidence::Medium);
        item.caveats.push(format!(
            "{parent} is inferred from the names of its partitions. PostgreSQL cannot create an index CONCURRENTLY on a partitioned table: this statement blocks writes while it runs. To avoid that, create the index CONCURRENTLY on each partition, then CREATE INDEX ON ONLY the parent and attach them."
        ));
        advice.push(item);
    }
    advice
}

/// The partitioned table's name: the common start of its partitions' names
/// (`events_2025_01`, `events_2025_02` → `events`).
fn parent_table(scans: &[&Node]) -> Option<String> {
    let names: Vec<&str> = scans
        .iter()
        .map(|scan| scan.relation_name.as_deref())
        .collect::<Option<_>>()?;
    let first = names[0];
    let mut length = first.len();
    for name in &names[1..] {
        length = first
            .bytes()
            .zip(name.bytes())
            .take(length)
            .take_while(|(a, b)| a == b)
            .count();
    }
    let prefix = first[..length].trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    (!prefix.is_empty() && names.iter().any(|name| *name != prefix)).then(|| prefix.to_owned())
}

/// A correlated subquery that scans a table once per outer row: index the
/// column it compares with the outer row.
pub fn correlated_subqueries(context: &Context) -> Vec<Advice> {
    let mut advice = Vec::new();
    for scan in &context.plan.nodes {
        if !scan.node_type.ends_with("Scan") {
            continue;
        }
        let Some(actuals) = scan.actuals.filter(|actuals| actuals.loops >= 2) else {
            continue;
        };
        if !in_subplan(context.plan, scan) {
            continue;
        }
        let removed = scan.rows_removed_by_filter;
        let heavily_filtered = removed >= 10.0 * actuals.rows.max(1.0) && removed >= 100.0;
        if scan.node_type != "Seq Scan" && !heavily_filtered {
            continue;
        }
        if context
            .inclusive_share(scan)
            .is_none_or(|share| share < MIN_SHARE)
        {
            continue;
        }
        let Some(filter) = scan.predicate(PredicateKind::Filter) else {
            continue;
        };
        let own = qualifier(scan);
        // Only the comparison with the outer row tells which rows to fetch.
        let mut correlated = Keys::default();
        correlated.add(filter, own, true);
        let mut plain = Keys::default();
        plain.add(filter, own, false);
        let Some((schema, name)) = table(scan) else {
            continue;
        };
        let Some(index) = correlated.index(schema.clone(), name.clone()) else {
            continue;
        };
        if plain.index(schema, name).as_ref() == Some(&index) {
            continue;
        }
        let loops = actuals.loops as f64;
        let summary = format!(
            "An index on {} would turn each of the {} runs of the subquery's {} into an index lookup.",
            index.target(),
            format::rows(loops),
            format::node(scan)
        );
        let mut evidence = vec![
            Evidence {
                label: "Runs",
                value: format::rows(loops),
            },
            Evidence {
                label: "Filter",
                value: filter.to_owned(),
            },
            kept_evidence(actuals.rows, actuals.rows + removed),
        ];
        if let Some(share) = context.inclusive_share(scan) {
            evidence.push(Evidence {
                label: "Time in the scans",
                value: format!("{} of the runtime", format::percent(share)),
            });
        }
        advice.push(index_advice(
            scan,
            index,
            &correlated,
            None,
            summary,
            evidence,
            Vec::new(),
        ));
    }
    advice
}

/// Whether a node runs inside a SubPlan, with no cache on the way.
fn in_subplan(plan: &Plan, node: &Node) -> bool {
    let mut current = node;
    loop {
        if matches!(current.node_type.as_str(), "Materialize" | "Memoize") {
            return false;
        }
        if current.relationship == Some(Relationship::SubPlan) {
            return true;
        }
        match current.parent {
            Some(parent) => current = plan.node(parent),
            None => return false,
        }
    }
}

/// Why the slow sequential scans that got no index got none.
pub fn explanations(context: &Context, advice: &[Advice]) -> Vec<Advice> {
    let advised: Vec<NodeId> = advice.iter().filter_map(|advice| advice.node).collect();
    let mut out = Vec::new();
    for scan in &context.plan.nodes {
        if scan.node_type != "Seq Scan" || advised.contains(&scan.id) {
            continue;
        }
        // Covered by the advice for its partitioned table.
        if scan.parent.is_some_and(|parent| advised.contains(&parent)) {
            continue;
        }
        let Some(share) = context.share(scan).filter(|&share| share >= MIN_SHARE) else {
            continue;
        };
        let Some(reason) = no_index_reason(context, scan) else {
            continue;
        };
        out.push(Advice {
            node: Some(scan.id),
            summary: format!("{}: {reason}", format::node(scan)),
            kind: AdviceKind::NoIndex { reason },
            confidence: Confidence::High,
            evidence: vec![Evidence {
                label: "Time in the node",
                value: format!("{} of the runtime", format::percent(share)),
            }],
            caveats: Vec::new(),
            verification: Verification::Unverified,
            rules: Vec::new(),
        });
    }
    out
}

fn no_index_reason(context: &Context, scan: &Node) -> Option<String> {
    let metrics = context.metrics.node(scan.id);
    let actuals = scan.actuals.filter(|actuals| !actuals.never_executed())?;
    let Some(filter) = scan.predicate(PredicateKind::Filter) else {
        return Some(
            "it reads the whole table because the query needs every row; there is no condition for an index to serve."
                .to_owned(),
        );
    };
    if metrics.may_stop_early {
        return Some(
            "a Limit or a semi join stops it after the first rows it needs, so it does not read the whole table."
                .to_owned(),
        );
    }
    let mut keys = Keys::default();
    keys.add(filter, qualifier(scan), false);
    if keys.or_across_columns {
        return Some(
            "the filter ORs conditions on different columns, which no single index serves. An index on each column would let PostgreSQL combine them with a BitmapOr."
                .to_owned(),
        );
    }
    if let Some(pages) = context
        .pages_per_scan(scan)
        .filter(|&pages| pages < MIN_PAGES)
    {
        return Some(format!(
            "the table is small ({}), so reading all of it is cheap.",
            format::pages(pages)
        ));
    }
    let read = actuals.rows + scan.rows_removed_by_filter;
    if read > 0.0 && actuals.rows / read >= MAX_SELECTIVITY {
        return Some(format!(
            "it keeps {} of the rows it reads; at that rate reading the whole table is cheaper than an index.",
            format::percent(actuals.rows / read)
        ));
    }
    None
}
