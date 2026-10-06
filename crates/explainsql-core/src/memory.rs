//! How much memory a sort or hash that spilled to disk would need, the
//! work_mem to suggest for it, and what that work_mem may cost: each sort
//! and hash of a statement may take that much, in each process that runs
//! it, in each session that runs the statement at the same time.

use serde_json::Value;

use crate::format;
use crate::ir::{Node, Plan};
use crate::metrics::Metrics;
use crate::scenario;

/// work_mem, in kilobytes: PostgreSQL's default, and the most suggested.
pub(crate) const DEFAULT_WORK_MEM: u64 = 4 * 1024;
pub(crate) const MAX_WORK_MEM: u64 = 1024 * 1024;

/// How much memory a spilled operation would need to stay in memory, in
/// kilobytes, by what the plan shows.
pub(crate) fn needed(node: &Node) -> Option<f64> {
    let number = |extra: &std::collections::BTreeMap<String, Value>, key: &str| {
        extra.get(key).and_then(Value::as_f64)
    };
    match node.node_type.as_str() {
        "Sort" | "Incremental Sort" => {
            // On disk, sorted rows take about a third of the room they
            // take in memory.
            let disk = std::iter::once(&node.extra)
                .chain(node.workers.iter().map(|worker| &worker.extra))
                .filter(|extra| {
                    extra.get("Sort Space Type").and_then(Value::as_str) == Some("Disk")
                })
                .filter_map(|extra| number(extra, "Sort Space Used"))
                .fold(0.0, f64::max);
            (disk > 0.0).then_some(disk * 3.0)
        }
        "Hash" => {
            let batches = node
                .extra_f64("Hash Batches")
                .filter(|&batches| batches > 1.0)?;
            // Each batch held about as much as the one in memory.
            Some(node.extra_f64("Peak Memory Usage")? * batches * 1.25)
        }
        "Aggregate" if matches!(node.strategy.as_deref(), Some("Hashed" | "Mixed")) => {
            let disk = node.extra_f64("Disk Usage").unwrap_or(0.0);
            let batches = node.extra_f64("HashAgg Batches").unwrap_or(0.0);
            (disk > 0.0 || batches > 1.0)
                .then(|| (node.extra_f64("Peak Memory Usage").unwrap_or(0.0) + disk) * 2.0)
        }
        _ => None,
    }
}

/// The work_mem to suggest for a spilled operation: a power of two
/// megabytes, enough for what it needs, more than the plan ran with, and at
/// most a gigabyte. `None` when it needs more, or when the plan ran with as
/// much already.
pub(crate) fn work_mem(plan: &Plan, node: &Node) -> Option<String> {
    let needed = needed(node)?;
    let mut megabytes: u64 = 1;
    // Kilobytes in the range of u64 are exact enough as floats here.
    #[allow(clippy::cast_precision_loss)]
    while ((megabytes * 1024) as f64) < needed && megabytes * 1024 < MAX_WORK_MEM {
        megabytes *= 2;
    }
    let kilobytes = megabytes * 1024;
    #[allow(clippy::cast_precision_loss)]
    let enough = kilobytes as f64 >= needed;
    (enough && kilobytes > current(plan)).then(|| {
        if megabytes >= 1024 {
            format!("{}GB", megabytes / 1024)
        } else {
            format!("{megabytes}MB")
        }
    })
}

/// `For this statement alone: SET LOCAL work_mem = '32MB' in its
/// transaction.`, with what that may take, when there is a work_mem to
/// suggest.
pub(crate) fn advice(plan: &Plan, metrics: &Metrics, node: &Node) -> Option<String> {
    let value = work_mem(plan, node)?;
    Some(format!(
        "For this statement alone: SET LOCAL work_mem = '{value}' in its transaction. {}",
        cost(plan, metrics, &value)
    ))
}

/// The work_mem the plan ran with, in kilobytes: its setting, or the
/// default.
fn current(plan: &Plan) -> u64 {
    plan.summary
        .settings
        .get("work_mem")
        .and_then(|value| scenario::kilobytes(value))
        .unwrap_or(DEFAULT_WORK_MEM)
}

/// Whether a node takes up to work_mem (or hash_mem) while it runs.
fn uses_work_mem(node: &Node) -> bool {
    match node.node_type.as_str() {
        "Sort" | "Incremental Sort" | "Hash" | "Memoize" | "Material" | "WindowAgg"
        | "Recursive Union" | "Bitmap Heap Scan" => true,
        "Aggregate" | "SetOp" => matches!(node.strategy.as_deref(), Some("Hashed" | "Mixed")),
        _ => false,
    }
}

/// What a work_mem of `value` may take for one execution of the plan: the
/// operations that each take up to work_mem, in each process that runs
/// them. Hash tables may take hash_mem_multiplier times more.
pub(crate) fn cost(plan: &Plan, metrics: &Metrics, value: &str) -> String {
    let operations: Vec<&Node> = plan
        .nodes
        .iter()
        .filter(|node| uses_work_mem(node))
        .collect();
    let allocations: f64 = operations
        .iter()
        .map(|node| metrics.node(node.id).processes.round().max(1.0))
        .sum();
    let kilobytes = scenario::kilobytes(value).unwrap_or(DEFAULT_WORK_MEM);
    #[allow(clippy::cast_precision_loss)]
    let total = format::kilobytes(allocations * kilobytes as f64);
    let count = operations.len().max(1);
    #[allow(clippy::cast_precision_loss)]
    let parallel = allocations > count as f64;
    match (count, parallel) {
        (1, false) => {
            "Every session that runs the statement at the same time may take that much.".to_owned()
        }
        (_, false) => format!(
            "Each of the plan's {count} sorts, hashes and other operations that use work_mem may take that much: up to {total} per execution, in every session that runs it at the same time."
        ),
        (_, true) => format!(
            "Each of the plan's {count} sorts, hashes and other operations that use work_mem may take that much in each process that runs it, {} in all: up to {total} per execution, in every session that runs it at the same time.",
            format::rows(allocations)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> Plan {
        crate::parse(text).unwrap()
    }

    const SPILL: &str = "\
Sort  (cost=38438.14..38938.14 rows=200000 width=37) (actual time=340.230..372.433 rows=200000 loops=1)
  Sort Key: orders.note
  Sort Method: external merge  Disk: 9272kB
  ->  Seq Scan on orders  (cost=0.00..4417.00 rows=200000 width=37) (actual time=0.014..22.127 rows=200000 loops=1)
Settings: work_mem = '64kB'";

    #[test]
    fn suggests_enough_work_mem() {
        let spill = plan(SPILL);
        // Three times the 9 MB written to disk: 32 MB.
        assert_eq!(needed(spill.root()), Some(9272.0 * 3.0));
        assert_eq!(work_mem(&spill, spill.root()).as_deref(), Some("32MB"));
        // Nothing to suggest for a sort that stayed in memory.
        let fits = plan(
            "Sort  (cost=1.00..2.00 rows=10 width=4) (actual time=0.010..0.020 rows=10 loops=1)\n  Sort Key: id\n  Sort Method: quicksort  Memory: 25kB\n  ->  Seq Scan on t  (cost=0.00..1.00 rows=10 width=4) (actual time=0.001..0.002 rows=10 loops=1)",
        );
        assert_eq!(work_mem(&fits, fits.root()), None);
        // Nor when the plan ran with that much already.
        let ran_with = plan(&SPILL.replace("'64kB'", "'1GB'"));
        assert_eq!(work_mem(&ran_with, ran_with.root()), None);
    }

    #[test]
    fn says_what_work_mem_may_take() {
        let spill = plan(SPILL);
        let metrics = crate::metrics::compute(&spill);
        assert_eq!(
            cost(&spill, &metrics, "32MB"),
            "Every session that runs the statement at the same time may take that much."
        );
        // A hash join and a sort, the hash in three processes.
        let parallel = plan(
            "\
Sort  (cost=10.00..11.00 rows=100 width=8) (actual time=5.000..5.100 rows=100 loops=1)
  Sort Key: o.id
  Sort Method: quicksort  Memory: 30kB
  ->  Gather  (cost=1.00..9.00 rows=100 width=8) (actual time=1.000..4.000 rows=100 loops=1)
        Workers Planned: 2
        Workers Launched: 2
        ->  Parallel Hash Join  (cost=1.00..8.00 rows=40 width=8) (actual time=1.000..3.000 rows=33 loops=3)
              Hash Cond: (o.customer_id = c.id)
              ->  Parallel Seq Scan on orders o  (cost=0.00..5.00 rows=40 width=8) (actual time=0.010..1.000 rows=33 loops=3)
              ->  Parallel Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=0.500..0.500 rows=3 loops=3)
                    Buckets: 1024  Batches: 1  Memory Usage: 40kB
                    ->  Parallel Seq Scan on customers c  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..0.100 rows=3 loops=3)",
        );
        let metrics = crate::metrics::compute(&parallel);
        assert_eq!(
            cost(&parallel, &metrics, "64MB"),
            "Each of the plan's 2 sorts, hashes and other operations that use work_mem may take that much in each process that runs it, 4 in all: up to 256.0 MB per execution, in every session that runs it at the same time."
        );
    }
}
