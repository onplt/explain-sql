//! Connected mode for ExplainSQL: running `EXPLAIN` safely (always inside a
//! transaction that is rolled back), reading catalog and statistics data for
//! the relations in a plan, and verifying suggested indexes with HypoPG or an
//! opt-in rolled-back `CREATE INDEX`.
//!
//! Nothing is implemented yet; see Phase 4 in `docs/ROADMAP.md`.
