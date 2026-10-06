//! The icicle view: the plan as nested boxes, the root on top and each node
//! under its parent, as wide as the time spent in it and below it. What is
//! too narrow for one column is folded into its parent and counted.

use explainsql_core::ir::{NodeId, Plan};

/// What the widths are proportional to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// CPU time, summed over parallel processes, so that workers fit
    /// under the node that gathers them.
    Time,
    /// The planner's estimated cost, for plans without timing.
    Cost,
}

/// A node's box on one row of the icicle.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub node: NodeId,
    /// The row: 0 for the node the view is zoomed on.
    pub depth: usize,
    /// The first column and the width, in columns.
    pub x: usize,
    pub width: usize,
    /// Nodes below this one too narrow to draw, with their descendants.
    pub folded: usize,
}

/// Each node's weight and the weight of its subtree, for one basis.
#[derive(Debug, Clone)]
pub struct Weights {
    pub basis: Basis,
    /// By node index.
    pub own: Vec<f64>,
    /// By node index.
    pub subtree: Vec<f64>,
}

impl Weights {
    /// By CPU time when the plan was timed, else by estimated cost. `cpu` is
    /// each node's own CPU time, by node index.
    pub fn new(plan: &Plan, cpu: &[Option<f64>]) -> Self {
        let timed = cpu.iter().any(|time| time.is_some_and(|time| time > 0.0));
        let own: Vec<f64> = if timed {
            cpu.iter()
                .map(|time| time.unwrap_or(0.0).max(0.0))
                .collect()
        } else {
            // A node's cost includes its children's: what is left is its own.
            plan.nodes
                .iter()
                .map(|node| {
                    let total = node.estimates.map_or(0.0, |e| e.total_cost);
                    let below: f64 = node
                        .children
                        .iter()
                        .filter_map(|&child| plan.node(child).estimates)
                        .map(|e| e.total_cost)
                        .sum();
                    (total - below).max(0.0)
                })
                .collect()
        };
        let mut subtree = own.clone();
        // Children before their parents: pre-order, reversed.
        for (_, node) in plan.walk().into_iter().rev() {
            if let Some(parent) = node.parent {
                subtree[parent.index()] += subtree[node.id.index()];
            }
        }
        Weights {
            basis: if timed { Basis::Time } else { Basis::Cost },
            own,
            subtree,
        }
    }

    /// The share of the plan's total spent in a node itself.
    pub fn own_share(&self, id: NodeId) -> f64 {
        let total = self.subtree.first().copied().unwrap_or(0.0);
        if total > 0.0 {
            self.own[id.index()] / total
        } else {
            0.0
        }
    }
}

/// The boxes of the subtree of `zoom` across `width` columns, row by row and
/// left to right. Children sit under their parent in plan order, from its
/// left edge; what their widths leave is the parent's own share.
pub fn layout(plan: &Plan, weights: &Weights, zoom: NodeId, width: usize) -> Vec<Cell> {
    let mut cells = vec![Cell {
        node: zoom,
        depth: 0,
        x: 0,
        width,
        folded: 0,
    }];
    let mut next = 0;
    while next < cells.len() {
        let parent = cells[next].clone();
        let node = plan.node(parent.node);
        let total = weights.subtree[parent.node.index()];
        let mut folded = 0;
        let mut cumulative = 0.0;
        for &child in &node.children {
            let weight = weights.subtree[child.index()];
            // Cumulative rounding: the children never take more than the
            // parent, and no column is counted twice.
            let start = column(cumulative, total, parent.width);
            cumulative += weight;
            let end = column(cumulative, total, parent.width);
            if end > start {
                cells.push(Cell {
                    node: child,
                    depth: parent.depth + 1,
                    x: parent.x + start,
                    width: end - start,
                    folded: 0,
                });
            } else {
                folded += subtree_size(plan, child);
            }
        }
        cells[next].folded = folded;
        next += 1;
    }
    cells
}

fn column(part: f64, total: f64, width: usize) -> usize {
    if total <= 0.0 {
        return 0;
    }
    ((part / total).clamp(0.0, 1.0) * width as f64).round() as usize
}

/// A node and everything below it.
pub fn subtree_size(plan: &Plan, id: NodeId) -> usize {
    let mut count = 0;
    let mut stack = vec![id];
    while let Some(id) = stack.pop() {
        count += 1;
        stack.extend(plan.node(id).children.iter().copied());
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights_of(plan: &Plan) -> Weights {
        let analysis = explainsql_core::analyze(plan);
        let cpu: Vec<Option<f64>> = analysis
            .metrics
            .nodes
            .iter()
            .map(|node| node.exclusive_cpu_time)
            .collect();
        Weights::new(plan, &cpu)
    }

    /// Children never overflow their parent, rows nest, and every node is
    /// drawn or counted as folded, at any width.
    fn check(plan: &Plan) {
        let weights = weights_of(plan);
        for width in [1, 7, 40, 80, 118, 300] {
            let cells = layout(plan, &weights, NodeId(0), width);
            for cell in &cells {
                assert!(cell.x + cell.width <= width, "{cell:?} at width {width}");
                assert!(cell.width >= 1);
                let Some(parent) = plan.node(cell.node).parent else {
                    continue;
                };
                let above = cells
                    .iter()
                    .find(|other| other.node == parent)
                    .expect("a drawn node's parent is drawn");
                assert_eq!(above.depth + 1, cell.depth);
                assert!(above.x <= cell.x && cell.x + cell.width <= above.x + above.width);
            }
            // Siblings do not overlap: on each row, boxes are disjoint.
            let mut rows: Vec<Vec<(usize, usize)>> = Vec::new();
            for cell in &cells {
                if rows.len() <= cell.depth {
                    rows.resize(cell.depth + 1, Vec::new());
                }
                rows[cell.depth].push((cell.x, cell.x + cell.width));
            }
            for row in &mut rows {
                row.sort();
                for pair in row.windows(2) {
                    assert!(pair[0].1 <= pair[1].0, "{pair:?} overlap at width {width}");
                }
            }
            let counted: usize = cells.iter().map(|cell| 1 + cell.folded).sum();
            assert_eq!(counted, plan.nodes.len(), "at width {width}");
        }
    }

    #[test]
    fn every_corpus_plan_nests_and_counts_every_node() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/pg");
        let mut checked = 0;
        for version in std::fs::read_dir(&dir).unwrap() {
            let version = version.unwrap().path();
            if !version.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&version).unwrap() {
                let file = file.unwrap().path();
                if file.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(plan) = explainsql_core::parse(&std::fs::read_to_string(&file).unwrap())
                else {
                    continue;
                };
                check(&plan);
                checked += 1;
            }
        }
        assert!(checked > 100, "{checked} plans");
    }

    #[test]
    fn widths_follow_time_and_cost() {
        let timed = explainsql_core::parse(
            "\
Hash Join  (cost=1.00..10.00 rows=10 width=8) (actual time=0.100..10.000 rows=10 loops=1)
  Hash Cond: (a.id = b.id)
  ->  Seq Scan on a  (cost=0.00..5.00 rows=100 width=4) (actual time=0.010..6.000 rows=100 loops=1)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4) (actual time=2.000..2.000 rows=10 loops=1)
        ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4) (actual time=0.010..2.000 rows=10 loops=1)
Execution Time: 10.100 ms",
        )
        .unwrap();
        let weights = weights_of(&timed);
        assert_eq!(weights.basis, Basis::Time);
        let cells = layout(&timed, &weights, NodeId(0), 100);
        let width = |id: u32| cells.iter().find(|c| c.node == NodeId(id)).unwrap().width;
        // 6 of 10 ms in the scan of a, 2 in the scan of b under the hash.
        assert_eq!(width(0), 100);
        assert_eq!(width(1), 60);
        assert_eq!(width(2), 20);
        assert_eq!(width(3), 20);

        let estimated = explainsql_core::parse(
            "\
Hash Join  (cost=1.00..10.00 rows=10 width=8)
  Hash Cond: (a.id = b.id)
  ->  Seq Scan on a  (cost=0.00..5.00 rows=100 width=4)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4)
        ->  Seq Scan on b  (cost=0.00..1.00 rows=10 width=4)",
        )
        .unwrap();
        let weights = weights_of(&estimated);
        assert_eq!(weights.basis, Basis::Cost);
        let cells = layout(&estimated, &weights, NodeId(0), 100);
        let width = |id: u32| cells.iter().find(|c| c.node == NodeId(id)).unwrap().width;
        assert_eq!(width(1), 50);
        assert_eq!(width(3), 10);
    }

    #[test]
    fn folds_what_is_too_narrow() {
        let mut text = String::from(
            "Append  (cost=0.00..10.00 rows=10 width=4) (actual time=0.010..100.000 rows=10 loops=1)\n  ->  Seq Scan on big  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..90.000 rows=1 loops=1)\n",
        );
        for n in 0..50 {
            text.push_str(&format!(
                "  ->  Seq Scan on small_{n}  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.100 rows=1 loops=1)\n"
            ));
        }
        let plan = explainsql_core::parse(&text).unwrap();
        let weights = weights_of(&plan);
        let cells = layout(&plan, &weights, NodeId(0), 80);
        assert!(cells[0].folded > 0, "{:?}", cells[0]);
        assert_eq!(cells.len() + cells[0].folded, plan.nodes.len());
        // Zoomed on a small scan, it takes the whole width.
        let small = plan.node(NodeId(0)).children[10];
        let cells = layout(&plan, &weights, small, 80);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].width, 80);
    }
}
