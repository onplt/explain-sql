//! The metrics engine against every plan in the corpus: exclusive times
//! must account for the whole tree without any node going negative, which
//! checks the handling of parallel query, CTEs, InitPlans, SubPlans and
//! rounding on real plans.

mod common;

use common::{corpus, plan_path, read};
use explainsql_core::{metrics, parse};

#[test]
fn exclusive_times_add_up_across_the_corpus() {
    let mut problems = Vec::new();
    let mut timed = 0;
    for (major, scenario) in corpus() {
        for extension in ["json", "txt"] {
            let label = format!("PostgreSQL {major} {scenario}.{extension}");
            let plan = parse(&read(&plan_path(major, &scenario, extension))).unwrap();
            let metrics = metrics::compute(&plan);
            for node in &plan.nodes {
                if metrics.node(node.id).inconsistent {
                    problems.push(format!(
                        "{label}: node {} ({}) took less time than its children",
                        node.id.0, node.node_type
                    ));
                }
            }
            let Some(tree) = metrics.statement.tree_time else {
                continue;
            };
            timed += 1;
            let sum: f64 = metrics
                .nodes
                .iter()
                .filter_map(|node| node.exclusive_time)
                .sum();
            if (sum - tree).abs() > 0.01 + tree * 1e-4 {
                problems.push(format!(
                    "{label}: exclusive times add up to {sum:.3} ms, the tree took {tree:.3} ms"
                ));
            }
            if let Some(execution) = metrics.statement.execution_time {
                if tree > execution + 0.01 {
                    problems.push(format!(
                        "{label}: the tree took {tree:.3} ms of a {execution:.3} ms execution"
                    ));
                }
            }
            let shares: f64 = metrics
                .nodes
                .iter()
                .filter_map(|node| node.time_share)
                .sum();
            if shares > 1.0 + 1e-6 {
                problems.push(format!("{label}: shares add up to {shares:.4}"));
            }
        }
    }
    assert!(timed > 800, "only {timed} timed plans");
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn parallel_plans_keep_wall_clock_and_cpu_time_apart() {
    for major in [13, 18] {
        let plan = parse(&read(&plan_path(major, "parallel_seq_scan", "txt"))).unwrap();
        let metrics = metrics::compute(&plan);
        let scan = plan
            .nodes
            .iter()
            .find(|node| node.node_type == "Seq Scan" && node.parallel_aware)
            .unwrap();
        let scan = metrics.node(scan.id);
        assert!(scan.processes > 1.0, "PostgreSQL {major}");
        let (wall, cpu) = (
            scan.inclusive_time.unwrap(),
            scan.inclusive_cpu_time.unwrap(),
        );
        assert!(
            (cpu - wall * scan.processes).abs() < 1e-9,
            "PostgreSQL {major}"
        );
    }
}
