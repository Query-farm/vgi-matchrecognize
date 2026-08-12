//! Bind-time type inference (spec §C). A total, bottom-up type-synthesis pass
//! over a measure/define [`Expr`], using input column types as leaves.
//!
//! Any expression that cannot be assigned a type is a clean [`MrError::Infer`]
//! (the worker turns this into a bind error pointing the user at the `type`
//! override escape hatch).

use crate::error::{MrError, Result};
use crate::expr::ast::{AggArg, AggKind, BinOp, Expr};
use crate::types::Ty;

/// What the inferencer needs to know about the surrounding query: the type of
/// each input column, and which identifiers are pattern variables.
pub trait BindSchema {
    /// Type of input column `name` (case-insensitive), if present.
    fn col_ty(&self, name: &str) -> Option<Ty>;
    /// Whether `name` (canonical upper case) is a declared pattern variable.
    fn is_variable(&self, name: &str) -> bool;
}

/// Infer the static [`Ty`] of `expr` under `schema`.
pub fn infer(expr: &Expr, schema: &dyn BindSchema) -> Result<Ty> {
    match expr {
        Expr::Null => Ok(Ty::Null),
        Expr::Bool(_) => Ok(Ty::Boolean),
        Expr::Int(_) => Ok(Ty::Int64),
        Expr::Double(_) => Ok(Ty::Double),
        Expr::Str(_) => Ok(Ty::Varchar),
        Expr::Interval { .. } => Ok(Ty::Interval),
        Expr::Col(name) => schema
            .col_ty(name)
            .ok_or_else(|| MrError::Bind(format!("unknown column '{name}'"))),
        Expr::Qualified(var, col) => {
            if !schema.is_variable(var) {
                return Err(MrError::Bind(format!(
                    "'{var}' is not a pattern variable (in '{var}.{col}')"
                )));
            }
            schema
                .col_ty(col)
                .ok_or_else(|| MrError::Bind(format!("unknown column '{col}' (in '{var}.{col}')")))
        }
        Expr::Nav { arg, .. } => infer(arg, schema),
        Expr::Agg { kind, arg } => infer_agg(*kind, arg, schema),
        Expr::Classifier(_) => Ok(Ty::Varchar),
        Expr::MatchNumber => Ok(Ty::Int64),
        Expr::RunningFinal { inner, .. } => infer(inner, schema),
        Expr::Neg(e) => {
            let t = infer(e, schema)?;
            if t.is_numeric() || t == Ty::Null {
                Ok(t)
            } else {
                Err(MrError::Infer(format!("cannot negate a {t:?} value")))
            }
        }
        Expr::Not(e) => expect_bool(e, schema, "NOT"),
        Expr::IsNull { .. } => Ok(Ty::Boolean),
        Expr::Between { expr, lo, hi, .. } => {
            // Operands must be comparable; result BOOLEAN.
            let _ = infer(expr, schema)?;
            let _ = infer(lo, schema)?;
            let _ = infer(hi, schema)?;
            Ok(Ty::Boolean)
        }
        Expr::In { expr, list, .. } => {
            let _ = infer(expr, schema)?;
            for e in list {
                let _ = infer(e, schema)?;
            }
            Ok(Ty::Boolean)
        }
        Expr::Cast { ty, .. } => Ok(ty.clone()),
        Expr::Call { name, args } => {
            let arg_tys = args
                .iter()
                .map(|a| infer(a, schema))
                .collect::<Result<Vec<_>>>()?;
            crate::engine::scalar::result_ty(name, &arg_tys)
        }
        Expr::Binary { op, lhs, rhs } => infer_binary(*op, lhs, rhs, schema),
    }
}

fn expect_bool(e: &Expr, schema: &dyn BindSchema, ctx: &str) -> Result<Ty> {
    let t = infer(e, schema)?;
    if t == Ty::Boolean || t == Ty::Null {
        Ok(Ty::Boolean)
    } else {
        Err(MrError::Infer(format!(
            "{ctx} requires a BOOLEAN operand, got {t:?}"
        )))
    }
}

fn infer_agg(kind: AggKind, arg: &AggArg, schema: &dyn BindSchema) -> Result<Ty> {
    match kind {
        AggKind::Count => Ok(Ty::Int64),
        AggKind::Sum
        | AggKind::Avg
        | AggKind::Min
        | AggKind::Max
        | AggKind::ArrayAgg
        | AggKind::Arbitrary => {
            let inner = match arg {
                AggArg::Expr(e) => infer(e, schema)?,
                AggArg::Star | AggArg::QualStar(_) => {
                    return Err(MrError::Infer(format!(
                        "{kind:?} does not accept a '*' argument"
                    )))
                }
            };
            match kind {
                AggKind::Sum => inner
                    .sum_ty()
                    .ok_or_else(|| MrError::Infer(format!("SUM of non-numeric {inner:?}"))),
                AggKind::Avg => inner
                    .avg_ty()
                    .ok_or_else(|| MrError::Infer(format!("AVG of non-numeric {inner:?}"))),
                // A list of whatever the argument is; an empty match yields an
                // empty list, so the element type still comes from the argument.
                AggKind::ArrayAgg => Ok(Ty::List(Box::new(inner))),
                AggKind::Min | AggKind::Max | AggKind::Arbitrary => Ok(inner),
                AggKind::Count => unreachable!(),
            }
        }
    }
}

fn infer_binary(op: BinOp, lhs: &Expr, rhs: &Expr, schema: &dyn BindSchema) -> Result<Ty> {
    use BinOp::*;
    let l = infer(lhs, schema)?;
    let r = infer(rhs, schema)?;
    match op {
        And | Or => {
            for (t, side) in [(l, "left"), (r, "right")] {
                if t != Ty::Boolean && t != Ty::Null {
                    return Err(MrError::Infer(format!(
                        "logical operator requires BOOLEAN operands, {side} is {t:?}"
                    )));
                }
            }
            Ok(Ty::Boolean)
        }
        Eq | Ne | Lt | Le | Gt | Ge => {
            // Comparable check: both numeric, both same temporal, both varchar,
            // both boolean, or NULL on either side.
            if comparable(&l, &r) {
                Ok(Ty::Boolean)
            } else {
                Err(MrError::Infer(format!("cannot compare {l:?} with {r:?}")))
            }
        }
        Concat => Ok(Ty::Varchar),
        Add => arith_add(l, r),
        Sub => arith_sub(l, r),
        Mul | Mod => arith_mul(l, r, op),
        Div => arith_div(l, r),
    }
}

fn comparable(l: &Ty, r: &Ty) -> bool {
    if *l == Ty::Null || *r == Ty::Null {
        return true;
    }
    if l.is_numeric() && r.is_numeric() {
        return true;
    }
    if l == r {
        return true;
    }
    // Same temporal family already covered by l == r.
    false
}

fn arith_add(l: Ty, r: Ty) -> Result<Ty> {
    // temporal + interval -> temporal
    if l.is_temporal() && r == Ty::Interval {
        return Ok(l);
    }
    if r.is_temporal() && l == Ty::Interval {
        return Ok(r);
    }
    if l == Ty::Interval && r == Ty::Interval {
        return Ok(Ty::Interval);
    }
    numeric_promote(l, r, "+")
}

fn arith_sub(l: Ty, r: Ty) -> Result<Ty> {
    if l.is_temporal() && r == Ty::Interval {
        return Ok(l);
    }
    if l.is_temporal() && r.is_temporal() && l == r {
        return Ok(Ty::Interval);
    }
    if l == Ty::Interval && r == Ty::Interval {
        return Ok(Ty::Interval);
    }
    numeric_promote(l, r, "-")
}

fn arith_mul(l: Ty, r: Ty, op: BinOp) -> Result<Ty> {
    let sym = if op == BinOp::Mod { "%" } else { "*" };
    numeric_promote(l, r, sym)
}

fn arith_div(l: Ty, r: Ty) -> Result<Ty> {
    // DECIMAL / DECIMAL stays DECIMAL; otherwise division widens to DOUBLE.
    if let (Ty::Decimal(p1, s1), Ty::Decimal(p2, s2)) = (&l, &r) {
        return Ok(Ty::Decimal(*p1.max(p2), *s1.max(s2)));
    }
    if (l.is_numeric() || l == Ty::Null) && (r.is_numeric() || r == Ty::Null) {
        Ok(Ty::Double)
    } else {
        Err(MrError::Infer(format!("cannot divide {l:?} by {r:?}")))
    }
}

fn numeric_promote(l: Ty, r: Ty, sym: &str) -> Result<Ty> {
    if (l.is_numeric() || l == Ty::Null) && (r.is_numeric() || r == Ty::Null) {
        l.unify(&r)
            .ok_or_else(|| MrError::Infer(format!("cannot apply '{sym}' to {l:?} and {r:?}")))
    } else {
        Err(MrError::Infer(format!(
            "cannot apply '{sym}' to {l:?} and {r:?}"
        )))
    }
}
