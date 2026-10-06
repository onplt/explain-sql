//! The engine of ExplainSQL: the plan IR, the PostgreSQL plan parsers, the
//! metrics engine, the rule engine and the index advisor.
//!
//! This crate performs no I/O and contains no async code, so that it can be
//! compiled to WebAssembly and tested deterministically. See
//! `docs/ARCHITECTURE.md` for the design.
//!
//! ```
//! let plan = explainsql_core::parse(
//!     "Seq Scan on orders  (cost=0.00..4917.00 rows=10 width=64) (actual time=1.2..12.6 rows=10 loops=1)\n  Filter: (customer_id = 4242)\n  Rows Removed by Filter: 199990",
//! )
//! .unwrap();
//! assert_eq!(plan.root().node_type, "Seq Scan");
//! assert_eq!(plan.root().relation_name.as_deref(), Some("orders"));
//! assert_eq!(plan.root().rows_removed_by_filter, 199990.0);
//! ```

pub mod advisor;
pub mod analysis;
pub mod catalog;
pub mod check;
pub mod compare;
pub mod counterfactual;
pub mod diff;
pub mod expr;
pub mod fingerprint;
pub mod format;
pub mod ir;
pub mod metrics;
pub mod params;
pub mod pg;
pub mod report;
pub mod rules;
pub mod scenario;

pub use analysis::{Analysis, analyze};
pub use pg::{ParseError, parse, parse_all};
