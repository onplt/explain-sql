//! Planner settings to plan a statement under, to see what the planner
//! would do otherwise: `enable_seqscan = off`, `random_page_cost = 1.1`,
//! `work_mem = 64MB`, `plan_cache_mode = force_generic_plan`.
//!
//! Only settings on a fixed list are accepted, each with a value of its
//! type. They change how a statement is planned and how much memory its
//! sorts and hashes may use, and nothing else. `explainsql-db` checks them
//! again and applies them with `set_config(…, true)`, which lasts until the
//! end of the transaction, and every transaction it runs is rolled back.

use std::fmt;

use serde::Serialize;

/// A planner setting and the value to plan with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Setting {
    pub name: String,
    pub value: String,
}

impl Setting {
    pub fn new(name: &str, value: &str) -> Self {
        Setting {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    /// Whether explainsql may apply the setting; why not, if not.
    pub fn check(&self) -> Result<(), String> {
        let Some((_, kind)) = ALLOWED.iter().find(|(name, _)| *name == self.name) else {
            return Err(format!(
                "explainsql changes only planner settings, not {}",
                self.name
            ));
        };
        if kind.accepts(&self.value) {
            Ok(())
        } else {
            Err(format!(
                "{} is not a value explainsql uses for {} ({})",
                self.value,
                self.name,
                kind.expects()
            ))
        }
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.name, self.value)
    }
}

/// The values a setting takes.
#[derive(Clone, Copy)]
enum Kind {
    /// `on` or `off`.
    Switch,
    /// A planner cost constant: a plain decimal number, at most 10⁶.
    Cost,
    /// `hash_mem_multiplier`: a plain decimal number from 1 to 1,000.
    Multiplier,
    /// An amount of memory with its unit (`kB`, `MB`, `GB`), up to the
    /// limit in kilobytes.
    Memory { max: u64 },
    /// A whole number in a range.
    Count { min: u64, max: u64 },
    /// `plan_cache_mode`.
    PlanCacheMode,
}

/// One gigabyte, in kilobytes: the most work_mem explainsql sets, since
/// each sort or hash of a measured run may use that much.
const GIGABYTE: u64 = 1024 * 1024;

/// The settings explainsql may change, and how.
const ALLOWED: [(&str, Kind); 37] = [
    ("enable_async_append", Kind::Switch),
    ("enable_bitmapscan", Kind::Switch),
    ("enable_gathermerge", Kind::Switch),
    ("enable_hashagg", Kind::Switch),
    ("enable_hashjoin", Kind::Switch),
    ("enable_incremental_sort", Kind::Switch),
    ("enable_indexonlyscan", Kind::Switch),
    ("enable_indexscan", Kind::Switch),
    ("enable_material", Kind::Switch),
    ("enable_memoize", Kind::Switch),
    ("enable_mergejoin", Kind::Switch),
    ("enable_nestloop", Kind::Switch),
    ("enable_parallel_append", Kind::Switch),
    ("enable_parallel_hash", Kind::Switch),
    ("enable_partition_pruning", Kind::Switch),
    ("enable_partitionwise_aggregate", Kind::Switch),
    ("enable_partitionwise_join", Kind::Switch),
    ("enable_presorted_aggregate", Kind::Switch),
    ("enable_seqscan", Kind::Switch),
    ("enable_sort", Kind::Switch),
    ("enable_tidscan", Kind::Switch),
    ("geqo", Kind::Switch),
    ("jit", Kind::Switch),
    ("cpu_index_tuple_cost", Kind::Cost),
    ("cpu_operator_cost", Kind::Cost),
    ("cpu_tuple_cost", Kind::Cost),
    ("parallel_setup_cost", Kind::Cost),
    ("parallel_tuple_cost", Kind::Cost),
    ("random_page_cost", Kind::Cost),
    ("seq_page_cost", Kind::Cost),
    ("hash_mem_multiplier", Kind::Multiplier),
    ("work_mem", Kind::Memory { max: GIGABYTE }),
    (
        "effective_cache_size",
        Kind::Memory {
            max: 1024 * 1024 * GIGABYTE,
        },
    ),
    ("from_collapse_limit", Kind::Count { min: 1, max: 100 }),
    ("join_collapse_limit", Kind::Count { min: 1, max: 100 }),
    (
        "max_parallel_workers_per_gather",
        Kind::Count { min: 0, max: 8 },
    ),
    ("plan_cache_mode", Kind::PlanCacheMode),
];

impl Kind {
    fn accepts(self, value: &str) -> bool {
        match self {
            Kind::Switch => matches!(value, "on" | "off"),
            Kind::Cost => decimal(value).is_some_and(|value| value <= 1.0e6),
            Kind::Multiplier => decimal(value).is_some_and(|value| (1.0..=1000.0).contains(&value)),
            Kind::Memory { max } => kilobytes(value).is_some_and(|kb| kb > 0 && kb <= max),
            Kind::Count { min, max } => value.parse::<u64>().is_ok_and(|count| {
                value.bytes().all(|b| b.is_ascii_digit()) && (min..=max).contains(&count)
            }),
            Kind::PlanCacheMode => {
                matches!(value, "auto" | "force_custom_plan" | "force_generic_plan")
            }
        }
    }

    fn expects(self) -> String {
        match self {
            Kind::Switch => "on or off".to_owned(),
            Kind::Cost => "a number up to 1,000,000".to_owned(),
            Kind::Multiplier => "a number from 1 to 1,000".to_owned(),
            Kind::Memory { max } => format!(
                "an amount with its unit, such as 64MB, up to {}",
                crate::format::kilobytes(max as f64)
            ),
            Kind::Count { min, max } => format!("a whole number from {min} to {max}"),
            Kind::PlanCacheMode => "auto, force_custom_plan or force_generic_plan".to_owned(),
        }
    }
}

/// A plain decimal number: digits, and at most one point between digits.
fn decimal(value: &str) -> Option<f64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, "0"));
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    (digits(whole) && digits(fraction) && whole.len() <= 12 && fraction.len() <= 12)
        .then(|| value.parse().ok())
        .flatten()
}

/// `64MB` in kilobytes. The unit is required, as PostgreSQL reads a bare
/// number in each setting's own unit.
pub(crate) fn kilobytes(value: &str) -> Option<u64> {
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    if number.is_empty() || number.len() > 12 {
        return None;
    }
    let factor = match unit {
        "kB" => 1,
        "MB" => 1024,
        "GB" => 1024 * 1024,
        "TB" => 1024 * 1024 * 1024,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, value: &str) -> Result<(), String> {
        Setting::new(name, value).check()
    }

    #[test]
    fn accepts_planner_settings() {
        for (name, value) in [
            ("enable_seqscan", "off"),
            ("enable_nestloop", "on"),
            ("random_page_cost", "1.1"),
            ("seq_page_cost", "1"),
            ("work_mem", "64MB"),
            ("work_mem", "4096kB"),
            ("work_mem", "1GB"),
            ("effective_cache_size", "16GB"),
            ("hash_mem_multiplier", "2.0"),
            ("join_collapse_limit", "8"),
            ("max_parallel_workers_per_gather", "0"),
            ("plan_cache_mode", "force_generic_plan"),
        ] {
            assert_eq!(check(name, value), Ok(()), "{name} = {value}");
        }
        assert_eq!(
            Setting::new("random_page_cost", "1.1").to_string(),
            "random_page_cost = 1.1"
        );
    }

    #[test]
    fn refuses_everything_else() {
        // Not planner settings.
        for name in [
            "statement_timeout",
            "search_path",
            "role",
            "session_authorization",
            "default_transaction_read_only",
            "transaction_read_only",
            "lock_timeout",
            "my.custom",
        ] {
            assert!(check(name, "on").is_err(), "{name}");
        }
        // Values of the wrong type, out of range, or that are not plain.
        for (name, value) in [
            ("enable_seqscan", "false"),
            ("enable_seqscan", "off; DROP TABLE orders"),
            ("random_page_cost", "-1"),
            ("random_page_cost", "1e3"),
            ("random_page_cost", "NaN"),
            ("random_page_cost", "inf"),
            ("random_page_cost", "1."),
            ("random_page_cost", ".5"),
            ("random_page_cost", "2000000"),
            ("work_mem", "64"),
            ("work_mem", "64mb"),
            ("work_mem", "2GB"),
            ("work_mem", "0MB"),
            ("work_mem", "99999999999999999999kB"),
            ("hash_mem_multiplier", "0.5"),
            ("join_collapse_limit", "0"),
            ("join_collapse_limit", "+8"),
            ("max_parallel_workers_per_gather", "64"),
            ("plan_cache_mode", "force"),
        ] {
            let error = check(name, value).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }
}
