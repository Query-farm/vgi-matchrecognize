//! Error types for the MATCH_RECOGNIZE engine.
//!
//! Everything is a `Result<_, MrError>`; the engine NEVER panics. The worker
//! maps each variant onto a clean DuckDB bind- or runtime-error.

use std::fmt;

/// An error raised by pattern/expression parsing, type inference, or matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MrError {
    /// A pattern (PATTERN clause) failed to parse.
    Pattern(String),
    /// A DEFINE/MEASURES expression failed to parse.
    Expr(String),
    /// Bind-time type inference could not assign a type (suggests an override).
    Infer(String),
    /// A reference to an unknown column or pattern variable, or an invalid
    /// construct caught at bind.
    Bind(String),
    /// A runtime evaluation error inside the matcher/evaluator.
    Eval(String),
    /// The per-partition step budget was exhausted (catastrophic backtracking).
    StepBudget(String),
}

impl MrError {
    /// Short machine-stable category label (used in messages/tests).
    pub fn kind(&self) -> &'static str {
        match self {
            MrError::Pattern(_) => "pattern",
            MrError::Expr(_) => "expr",
            MrError::Infer(_) => "infer",
            MrError::Bind(_) => "bind",
            MrError::Eval(_) => "eval",
            MrError::StepBudget(_) => "step_budget",
        }
    }
}

impl fmt::Display for MrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MrError::Pattern(m) => write!(f, "match_recognize pattern error: {m}"),
            MrError::Expr(m) => write!(f, "match_recognize expression error: {m}"),
            MrError::Infer(m) => write!(f, "match_recognize type-inference error: {m}"),
            MrError::Bind(m) => write!(f, "match_recognize bind error: {m}"),
            MrError::Eval(m) => write!(f, "match_recognize evaluation error: {m}"),
            MrError::StepBudget(m) => write!(f, "match_recognize: {m}"),
        }
    }
}

impl std::error::Error for MrError {}

/// Convenience result alias for the crate.
pub type Result<T> = std::result::Result<T, MrError>;
