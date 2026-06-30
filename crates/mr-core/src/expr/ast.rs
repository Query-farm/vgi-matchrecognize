//! The expression AST shared by DEFINE and MEASURES. The same tree feeds the
//! type inferencer ([`infer`](fn@crate::types::infer)) and the evaluator
//! ([`crate::engine::eval`]).

use crate::types::Ty;

/// Physical (`PREV`/`NEXT`) and logical (`FIRST`/`LAST`) navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKind {
    /// `PREV` — n physical rows before the current row on the tape.
    Prev,
    /// `NEXT` — n physical rows after the current row on the tape.
    Next,
    /// `FIRST` — the (n-th from) first matched row in the frame/variable.
    First,
    /// `LAST` — the (n-th from) last matched row in the frame/variable.
    Last,
}

/// Running aggregate kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// The argument to an aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum AggArg {
    /// `COUNT(*)` — count of all matched rows in the frame.
    Star,
    /// `COUNT(A.*)` — count of rows bound to variable `A` in the frame.
    QualStar(String),
    /// An ordinary expression argument.
    Expr(Box<Expr>),
}

/// An expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    /// `INTERVAL n UNIT` as `(months, days, nanos)`.
    Interval {
        months: i32,
        days: i32,
        nanos: i64,
    },
    /// An unqualified column reference (the current row in the frame).
    Col(String),
    /// A variable-qualified column reference `VAR.col`.
    Qualified(String, String),
    /// `PREV/NEXT/FIRST/LAST(expr [, n])`.
    Nav {
        kind: NavKind,
        arg: Box<Expr>,
        offset: usize,
    },
    /// `SUM/COUNT/AVG/MIN/MAX(arg)`.
    Agg {
        kind: AggKind,
        arg: AggArg,
    },
    /// `CLASSIFIER()`.
    Classifier,
    /// `MATCH_NUMBER()`.
    MatchNumber,
    /// `RUNNING(inner)` / `FINAL(inner)`.
    RunningFinal {
        final_: bool,
        inner: Box<Expr>,
    },
    /// Unary minus.
    Neg(Box<Expr>),
    /// `NOT expr`.
    Not(Box<Expr>),
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `expr [NOT] BETWEEN lo AND hi`.
    Between {
        expr: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        negated: bool,
    },
    /// `expr [NOT] IN (list...)`.
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `CAST(expr AS T)` / `expr::T`.
    Cast {
        expr: Box<Expr>,
        ty: Ty,
    },
}
