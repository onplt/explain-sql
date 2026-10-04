//! The engine of ExplainSQL: the plan IR, the PostgreSQL plan parsers, the
//! metrics engine, the rule engine and the index advisor.
//!
//! This crate performs no I/O and contains no async code, so that it can be
//! compiled to WebAssembly and tested deterministically. See
//! `docs/ARCHITECTURE.md` for the design.
//!
//! Nothing is implemented yet. The parsers arrive in Phase 1 of the roadmap and
//! are developed against the fixture corpus in `fixtures/`.
