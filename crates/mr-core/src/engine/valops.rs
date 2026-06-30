//! Runtime value operations: arithmetic, comparison, 3-valued logic, string
//! concat, and coercion of a [`Value`] to a target [`Ty`] (used for CAST and
//! for the final per-column output coercion).
//!
//! The evaluator computes "best effort" values (pure-integer arithmetic stays
//! integral; anything involving floats/decimals widens to `Double`), then the
//! output builder coerces each measure value to its inferred column `Ty`. Exact
//! DECIMAL arithmetic is a documented v1.1 / `type`-override corner.

use crate::error::{MrError, Result};
use crate::expr::ast::BinOp;
use crate::types::{TimeUnit, Ty};
use crate::value::{Interval, Value};

/// Nanoseconds per one tick of `unit`.
fn unit_nanos(unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Milli => 1_000_000,
        TimeUnit::Micro => 1_000,
        TimeUnit::Nano => 1,
    }
}

/// Total ordering comparison of two non-null values, if comparable.
pub fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Interval(x), Interval(y)) => Some(interval_nanos(x).cmp(&interval_nanos(y))),
        (Timestamp(x, ux), Timestamp(y, uy)) => {
            Some((*x * unit_nanos(*ux)).cmp(&(*y * unit_nanos(*uy))))
        }
        (Time(x, ux), Time(y, uy)) => Some((*x * unit_nanos(*ux)).cmp(&(*y * unit_nanos(*uy)))),
        _ => {
            // Numeric comparison.
            let (x, y) = (a.as_f64()?, b.as_f64()?);
            x.partial_cmp(&y).or(Some(Ordering::Equal))
        }
    }
}

fn interval_nanos(i: &Interval) -> i128 {
    // Approximate calendar parts (month = 30 days, used only for ordering).
    (i.months as i128) * 30 * 86_400 * 1_000_000_000
        + (i.days as i128) * 86_400 * 1_000_000_000
        + i.nanos as i128
}

/// Apply a binary operator to two evaluated values with 3-valued logic.
pub fn binary(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    use BinOp::*;
    match op {
        And => Ok(logic_and(l, r)),
        Or => Ok(logic_or(l, r)),
        Eq | Ne | Lt | Le | Gt | Ge => Ok(compare_op(op, l, r)),
        Concat => Ok(concat(l, r)),
        Add | Sub | Mul | Div | Mod => arith(op, l, r),
    }
}

fn logic_and(l: &Value, r: &Value) -> Value {
    match (l.as_bool(), r.as_bool()) {
        (Some(false), _) | (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}

fn logic_or(l: &Value, r: &Value) -> Value {
    match (l.as_bool(), r.as_bool()) {
        (Some(true), _) | (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}

fn compare_op(op: BinOp, l: &Value, r: &Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let ord = match compare(l, r) {
        Some(o) => o,
        None => return Value::Null,
    };
    use std::cmp::Ordering::*;
    let res = match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Value::Bool(res)
}

fn concat(l: &Value, r: &Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    Value::Str(format!("{}{}", to_string(l), to_string(r)))
}

/// A display string for a value (used by `||` and CAST-to-VARCHAR).
pub fn to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Decimal(u, s) => format_decimal(*u, *s),
        Value::Str(s) => s.clone(),
        Value::Date(d) => format!("date:{d}"),
        Value::Timestamp(t, _) => format!("ts:{t}"),
        Value::Time(t, _) => format!("time:{t}"),
        Value::Interval(i) => format!("{}mo {}d {}ns", i.months, i.days, i.nanos),
    }
}

fn format_decimal(unscaled: i128, scale: i8) -> String {
    if scale <= 0 {
        return unscaled.to_string();
    }
    let neg = unscaled < 0;
    let mag = unscaled.unsigned_abs();
    let s = mag.to_string();
    let scale = scale as usize;
    let s = if s.len() <= scale {
        format!("0.{:0>width$}", s, width = scale)
    } else {
        let (int, frac) = s.split_at(s.len() - scale);
        format!("{int}.{frac}")
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn arith(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    // Temporal arithmetic.
    if let Some(v) = temporal_arith(op, l, r) {
        return Ok(v);
    }
    // Pure integer arithmetic stays integral.
    if let (Some(x), Some(y)) = (l.as_i128(), r.as_i128()) {
        let out = match op {
            BinOp::Add => x.checked_add(y),
            BinOp::Sub => x.checked_sub(y),
            BinOp::Mul => x.checked_mul(y),
            BinOp::Mod => {
                if y == 0 {
                    return Ok(Value::Null);
                }
                Some(x % y)
            }
            BinOp::Div => {
                // Division widens to DOUBLE.
                if y == 0 {
                    return Ok(Value::Null);
                }
                return Ok(Value::Double(x as f64 / y as f64));
            }
            _ => unreachable!(),
        };
        let out = out.ok_or_else(|| MrError::Eval("integer arithmetic overflow".into()))?;
        return Ok(if (i64::MIN as i128..=i64::MAX as i128).contains(&out) {
            Value::Int(out as i64)
        } else {
            Value::HugeInt(out)
        });
    }
    // Floating / decimal path -> DOUBLE.
    let (x, y) = (
        l.as_f64()
            .ok_or_else(|| MrError::Eval(format!("non-numeric operand {l:?}")))?,
        r.as_f64()
            .ok_or_else(|| MrError::Eval(format!("non-numeric operand {r:?}")))?,
    );
    let out = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0.0 {
                return Ok(Value::Null);
            }
            x / y
        }
        BinOp::Mod => {
            if y == 0.0 {
                return Ok(Value::Null);
            }
            x % y
        }
        _ => unreachable!(),
    };
    Ok(Value::Double(out))
}

fn temporal_arith(op: BinOp, l: &Value, r: &Value) -> Option<Value> {
    match (op, l, r) {
        (BinOp::Add, Value::Timestamp(t, u), Value::Interval(i))
        | (BinOp::Add, Value::Interval(i), Value::Timestamp(t, u)) => {
            Some(Value::Timestamp(t + interval_in_unit(i, *u), *u))
        }
        (BinOp::Sub, Value::Timestamp(t, u), Value::Interval(i)) => {
            Some(Value::Timestamp(t - interval_in_unit(i, *u), *u))
        }
        (BinOp::Add, Value::Date(d), Value::Interval(i))
        | (BinOp::Add, Value::Interval(i), Value::Date(d)) => {
            Some(Value::Date(d + interval_in_days(i)))
        }
        (BinOp::Sub, Value::Date(d), Value::Interval(i)) => {
            Some(Value::Date(d - interval_in_days(i)))
        }
        (BinOp::Sub, Value::Timestamp(a, ua), Value::Timestamp(b, ub)) => {
            let nanos = a * unit_nanos(*ua) - b * unit_nanos(*ub);
            Some(Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos,
            }))
        }
        _ => None,
    }
}

fn interval_in_unit(i: &Interval, unit: TimeUnit) -> i64 {
    // Calendar parts approximated (month = 30 days); exact only for time parts.
    let total_nanos = (i.months as i64) * 30 * 86_400 * 1_000_000_000
        + (i.days as i64) * 86_400 * 1_000_000_000
        + i.nanos;
    total_nanos / unit_nanos(unit)
}

fn interval_in_days(i: &Interval) -> i32 {
    i.months * 30 + i.days + (i.nanos / (86_400 * 1_000_000_000)) as i32
}

/// Negate a numeric value (3-valued).
pub fn negate(v: &Value) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => Ok(Value::Int(-i)),
        Value::HugeInt(i) => Ok(Value::HugeInt(-i)),
        Value::Double(d) => Ok(Value::Double(-d)),
        Value::Decimal(u, s) => Ok(Value::Decimal(-u, *s)),
        other => Err(MrError::Eval(format!("cannot negate {other:?}"))),
    }
}

/// Coerce a value to a target type (CAST and final output coercion).
pub fn coerce(v: Value, ty: Ty) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match ty {
        Ty::Boolean => v
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| MrError::Eval(format!("cannot cast {v:?} to BOOLEAN"))),
        Ty::Int64 => match &v {
            Value::Int(_) => Ok(v),
            Value::HugeInt(i) => Ok(Value::Int(*i as i64)),
            Value::Double(d) => Ok(Value::Int(*d as i64)),
            Value::Decimal(u, s) => Ok(Value::Int((*u / 10i128.pow(*s as u32)) as i64)),
            Value::Bool(b) => Ok(Value::Int(*b as i64)),
            Value::Str(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| MrError::Eval(format!("cannot cast '{s}' to BIGINT"))),
            _ => Err(MrError::Eval(format!("cannot cast {v:?} to BIGINT"))),
        },
        Ty::HugeInt => v
            .as_i128()
            .map(Value::HugeInt)
            .or_else(|| v.as_f64().map(|d| Value::HugeInt(d as i128)))
            .ok_or_else(|| MrError::Eval(format!("cannot cast {v:?} to HUGEINT"))),
        Ty::Double => v
            .as_f64()
            .map(Value::Double)
            .ok_or_else(|| MrError::Eval(format!("cannot cast {v:?} to DOUBLE"))),
        Ty::Decimal(_, s) => {
            let f = v
                .as_f64()
                .ok_or_else(|| MrError::Eval(format!("cannot cast {v:?} to DECIMAL")))?;
            let unscaled = (f * 10f64.powi(s as i32)).round() as i128;
            Ok(Value::Decimal(unscaled, s))
        }
        Ty::Varchar => Ok(Value::Str(to_string(&v))),
        Ty::Date => match v {
            Value::Date(_) => Ok(v),
            other => Err(MrError::Eval(format!("cannot cast {other:?} to DATE"))),
        },
        Ty::Timestamp(_) => match v {
            Value::Timestamp(..) => Ok(v),
            other => Err(MrError::Eval(format!("cannot cast {other:?} to TIMESTAMP"))),
        },
        Ty::Time(_) => match v {
            Value::Time(..) => Ok(v),
            other => Err(MrError::Eval(format!("cannot cast {other:?} to TIME"))),
        },
        Ty::Interval => match v {
            Value::Interval(_) => Ok(v),
            other => Err(MrError::Eval(format!("cannot cast {other:?} to INTERVAL"))),
        },
        Ty::Null => Ok(Value::Null),
    }
}
