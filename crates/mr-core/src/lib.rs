//! `mr-core` — a pure-compute SQL:2016 `MATCH_RECOGNIZE` engine.
//!
//! This crate has **no Arrow and no VGI dependency**: it parses the PATTERN and
//! DEFINE/MEASURES clauses, infers the output measure types at bind time, and
//! runs a backtracking row-pattern matcher over the [`RowStore`] trait — so the
//! whole engine is unit- and property-testable with a plain in-memory store.
//!
//! # Pipeline
//!
//! 1. [`pattern::parse`] → [`pattern::Pattern`] → [`compile`](fn@crate::pattern::compile) →
//!    a backtracking-VM [`pattern::Program`].
//! 2. [`expr::parse`] (a Pratt parser) → [`expr::Expr`] for each DEFINE
//!    predicate and MEASURES expression.
//! 3. [`infer`](fn@crate::types::infer) → the output [`types::Ty`] of each measure (spec §C).
//! 4. [`plan::Plan::build`] ties 1–3 together at bind time and computes the
//!    output column layout; [`plan::Plan::run`] groups/sorts/matches/evaluates
//!    at produce time.
//!
//! `mr-worker` provides the Arrow-backed [`RowStore`] and the `Ty -> DataType`
//! mapping; everything else lives here.
//!
//! [`RowStore`]: engine::RowStore

pub mod diag;
pub mod engine;
pub mod error;
pub mod expr;
pub mod pattern;
pub mod plan;
pub mod types;
pub mod value;

pub use error::{MrError, Result};
pub use plan::{Plan, PlanConfig};

/// The crate (worker) build version string, published as the catalog's
/// `implementation_version`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parse a pattern and return its human-readable compiled form (the
/// `explain_pattern` developer aid). No data access.
pub fn explain_pattern(src: &str) -> Result<String> {
    let pat = pattern::parse(src)?;
    Ok(pattern::explain(&pat))
}
