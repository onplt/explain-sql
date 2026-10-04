//! Numbers and node names as people read them, shared by findings and
//! reports.

use crate::ir::Node;

/// A row count: `200,000`, or `2.5` for the fractional averages of
/// PostgreSQL 18.
pub fn rows(count: f64) -> String {
    if count.fract() == 0.0 && count.abs() < 1e15 {
        // Integral and in range, so the conversion is exact.
        #[allow(clippy::cast_possible_truncation)]
        let whole = count as i64;
        grouped(whole)
    } else {
        format!("{count:.2}")
    }
}

/// An integer with thousands separators.
pub fn grouped(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A duration given in milliseconds: `0.042 ms`, `11.9 ms`, `1.84 s`.
pub fn duration(ms: f64) -> String {
    if ms < 1.0 {
        format!("{ms:.3} ms")
    } else if ms < 10.0 {
        format!("{ms:.2} ms")
    } else if ms < 1000.0 {
        format!("{ms:.1} ms")
    } else if ms < 60_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{:.1} min", ms / 60_000.0)
    }
}

/// A fraction as a percentage with enough digits to tell small values apart:
/// `94%`, `4.2%`, `0.005%`.
pub fn percent(fraction: f64) -> String {
    let percent = fraction * 100.0;
    if percent >= 10.0 {
        format!("{percent:.0}%")
    } else if percent >= 1.0 {
        format!("{percent:.1}%")
    } else if percent >= 0.1 {
        format!("{percent:.2}%")
    } else if percent >= 0.001 {
        format!("{percent:.3}%")
    } else if percent > 0.0 {
        "<0.001%".to_owned()
    } else {
        "0%".to_owned()
    }
}

/// A size given in kilobytes: `512 kB`, `9.1 MB`, `2.3 GB`.
pub fn kilobytes(kb: f64) -> String {
    if kb < 1024.0 {
        format!("{kb:.0} kB")
    } else if kb < 1024.0 * 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{:.1} GB", kb / (1024.0 * 1024.0))
    }
}

/// A number of 8 kB pages with its size: `2,417 pages (19 MB)`.
pub fn pages(count: f64) -> String {
    format!(
        "{} pages ({})",
        rows(count.round()),
        kilobytes(count.round() * 8.0)
    )
}

/// How many times larger: `20×`, `3.5×`.
pub fn factor(value: f64) -> String {
    if value >= 100.0 {
        format!("{}×", rows(value.round()))
    } else if value >= 10.0 {
        format!("{value:.0}×")
    } else {
        format!("{value:.1}×")
    }
}

/// A short name for a node: `Seq Scan on orders`, `Hash Join`,
/// `Index Scan using orders_pkey on orders o`.
pub fn node(node: &Node) -> String {
    let mut name = String::new();
    if let Some(subplan) = &node.subplan_name {
        name.push_str(&format!("[{subplan}] "));
    }
    if node.parallel_aware {
        name.push_str("Parallel ");
    }
    // The names the text format uses.
    let kind = node.join_type.as_deref().filter(|&kind| kind != "Inner");
    let partial = node
        .partial_mode
        .as_deref()
        .filter(|&mode| mode != "Simple")
        .map(|mode| format!("{mode} "))
        .unwrap_or_default();
    let display = match (node.node_type.as_str(), kind, node.strategy.as_deref()) {
        ("Nested Loop", Some(kind), _) => format!("Nested Loop {kind} Join"),
        ("Hash Join", Some(kind), _) => format!("Hash {kind} Join"),
        ("Merge Join", Some(kind), _) => format!("Merge {kind} Join"),
        ("Aggregate", _, Some("Hashed")) => format!("{partial}HashAggregate"),
        ("Aggregate", _, Some("Sorted")) => format!("{partial}GroupAggregate"),
        ("Aggregate", _, Some("Mixed")) => format!("{partial}MixedAggregate"),
        ("Aggregate", _, _) => format!("{partial}Aggregate"),
        ("SetOp", _, strategy) => {
            let prefix = if strategy == Some("Hashed") {
                "HashSetOp"
            } else {
                "SetOp"
            };
            match &node.command {
                Some(command) => format!("{prefix} {command}"),
                None => prefix.to_owned(),
            }
        }
        ("ModifyTable", _, _) => node
            .operation
            .clone()
            .unwrap_or_else(|| "ModifyTable".to_owned()),
        (node_type, _, _) => node_type.to_owned(),
    };
    name.push_str(&display);
    if let Some(index) = &node.index_name {
        name.push_str(&format!(" using {index}"));
    }
    let object = node
        .relation_name
        .as_deref()
        .or(node.cte_name.as_deref())
        .or(node.function_name.as_deref());
    match (object, node.alias.as_deref()) {
        (Some(object), Some(alias)) if alias != object => {
            name.push_str(&format!(" on {object} {alias}"));
        }
        (Some(object), _) => name.push_str(&format!(" on {object}")),
        (None, Some(alias)) => name.push_str(&format!(" on {alias}")),
        (None, None) => {}
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(rows(200_000.0), "200,000");
        assert_eq!(rows(2.5), "2.50");
        assert_eq!(grouped(-1_234_567), "-1,234,567");
        assert_eq!(duration(0.0421), "0.042 ms");
        assert_eq!(duration(11.89), "11.9 ms");
        assert_eq!(duration(1840.0), "1.84 s");
        assert_eq!(percent(0.94), "94%");
        assert_eq!(percent(0.00005), "0.005%");
        assert_eq!(kilobytes(9272.0), "9.1 MB");
        assert_eq!(pages(2417.0), "2,417 pages (18.9 MB)");
        assert_eq!(factor(20.0), "20×");
        assert_eq!(factor(50_000.0), "50,000×");
    }

    #[test]
    fn names_nodes() {
        let plan = crate::parse(
            "\
Hash Right Anti Join  (cost=1.00..10.00 rows=1 width=8)
  Hash Cond: (a.id = b.id)
  ->  Seq Scan on orders o  (cost=0.00..5.00 rows=10 width=4)
  ->  Hash  (cost=1.00..1.00 rows=10 width=4)
        ->  Index Scan using b_pkey on b  (cost=0.00..1.00 rows=10 width=4)",
        )
        .unwrap();
        let names: Vec<String> = plan.nodes.iter().map(node).collect();
        assert_eq!(
            names,
            [
                "Hash Right Anti Join",
                "Seq Scan on orders o",
                "Hash",
                "Index Scan using b_pkey on b"
            ]
        );
    }
}
