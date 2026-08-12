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

/// SQL comparison of two values: `None` means "unordered", which every caller
/// turns into SQL NULL rather than into an ordering.
///
/// This is **not** a total order — see [`cmp_for_sort`] for that. NULL is
/// unordered against everything, and so is NaN, because SQL comparison against
/// an unknown is unknown rather than false.
///
/// Two things here used to be answered through `as_f64`, and both were wrong:
///
/// - **Integers.** `f64` has 53 bits of mantissa, so every pair of `BIGINT`s
///   past 2^53 collapsed onto one float: `9007199254740993 = 9007199254740992`
///   was TRUE, and `1700000000000000001 > 1700000000000000000` was FALSE.
///   Nanosecond epochs and snowflake ids live in exactly that range. Worse, the
///   *sort* comparator had already been fixed to compare integers exactly, so
///   `ORDER BY` and a DEFINE predicate disagreed about whether two rows were
///   equal. [`cmp_ints`] is now shared by both, and `tests/compare.rs`
///   (`comparators_agree_exactly`) pins them together.
/// - **NaN.** `partial_cmp(...).or(Some(Equal))` reported NaN as *equal to
///   everything*, so `NaN = 1.0` was TRUE and `NaN <> 1.0` was FALSE.
pub fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Interval(x), Interval(y)) => Some(interval_nanos(x).cmp(&interval_nanos(y))),
        // Rescale in i128, and skip rescaling entirely when the units already
        // match (the normal case: one column carries one unit). Multiplying i64
        // ticks by the unit scale overflows — microsecond ticks past ~year 2262
        // wrap, so `TIMESTAMP '9999-12-31'` compared as *less* than 2020, which
        // silently reversed ORDER BY and corrupted every match in the partition.
        (Timestamp(x, ux), Timestamp(y, uy)) => Some(if ux == uy {
            x.cmp(y)
        } else {
            (*x as i128 * unit_nanos(*ux) as i128).cmp(&(*y as i128 * unit_nanos(*uy) as i128))
        }),
        (Time(x, ux), Time(y, uy)) => Some(if ux == uy {
            x.cmp(y)
        } else {
            (*x as i128 * unit_nanos(*ux) as i128).cmp(&(*y as i128 * unit_nanos(*uy) as i128))
        }),
        // Exact within the integer family, before anything can reach `as_f64`.
        _ => match cmp_ints(a, b) {
            Some(ord) => Some(ord),
            // Anything else numeric: DOUBLE, DECIMAL, and the mixed pairs.
            // `partial_cmp` yields None for NaN, which is the right answer —
            // unordered, hence SQL NULL.
            None => a.as_f64()?.partial_cmp(&b.as_f64()?),
        },
    }
}

/// Exact ordering within the integer family (`BIGINT` / `HUGEINT`), or `None`
/// when either side is outside it.
///
/// Widening to `i128` is lossless for every `i64`, which is the whole point:
/// `as_f64` is not, and collapses adjacent values past 2^53 onto one float.
///
/// Shared by [`compare`] (SQL semantics) and [`cmp_present`] (the total sort
/// order) so the two cannot answer differently. A divergence there sorts the
/// tape differently from what a DEFINE predicate sees, which is not a wrong
/// output value but a wrong *match*.
///
/// DECIMAL against an integer is deliberately left out and still goes through
/// `as_f64` — a pre-existing precision wart, not widened here.
fn cmp_ints(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        // Across signedness too: an i64 and a u64 share no Rust integer type,
        // but i128 contains both ranges exactly, so `Int(-1)` vs
        // `UInt(u64::MAX)` orders correctly rather than through f64 (where both
        // u64::MAX and u64::MAX - 1 are the same value).
        (
            Value::Int(_) | Value::UInt(_) | Value::HugeInt(_),
            Value::Int(_) | Value::UInt(_) | Value::HugeInt(_),
        ) => Some(a.as_i128()?.cmp(&b.as_i128()?)),
        _ => None,
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
        // A list renders like DuckDB's list literal: [a, b, c].
        Value::List(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|i| {
                    if i.is_null() {
                        "NULL".to_string()
                    } else {
                        to_string(i)
                    }
                })
                .collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(i) => i.to_string(),
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

/// `10^scale` as the divisor that turns an unscaled decimal into a whole number.
///
/// `scale` is an `i8` and Arrow permits it to be negative (a `DECIMAL(p, -2)`
/// counts hundreds), which `10i128.pow(scale as u32)` turned into a ~4e9
/// exponent and a panic. A non-positive scale means the unscaled value is
/// *already* integral or larger, so the divisor is 1 — the same reading
/// `format_decimal` has always had for `scale <= 0`.
///
/// Clamped at 38, the widest DECIMAL Arrow can carry, so the exponent cannot
/// overflow i128 either.
pub(crate) fn pow10(scale: i8) -> i128 {
    if scale <= 0 {
        return 1;
    }
    10i128.pow(scale.min(38) as u32)
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
    // Temporal arithmetic. `None` means "not a temporal combination" and falls
    // through to the numeric paths; `Some(Err(_))` means it *was* one and went
    // out of range, which must not be confused with the former.
    if let Some(v) = temporal_arith(op, l, r) {
        return v;
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

/// Nanoseconds in one day — the exact conversion every consumer of
/// [`Interval::days`] uses, which is what makes spilling days lossless below.
const NS_PER_DAY: i128 = 86_400 * 1_000_000_000;

/// Temporal arithmetic, or `None` when the operands are not a temporal
/// combination (the caller then falls through to the numeric paths).
///
/// **Everything here computes in i128.** Rescaling i64 ticks to nanoseconds
/// overflows well inside the representable timestamp range: i64 nanoseconds run
/// out around 2262, so `TIMESTAMP '9999-12-31'` in microseconds is 2.5e20
/// against a ceiling of 9.2e18. That was a panic under `cargo test` and a
/// silently wrapped — negative — interval in release, where nothing sets
/// `overflow-checks`. `compare` documents the same trap for ordering; this is
/// the arithmetic half of it.
///
/// Genuine out-of-range is an error rather than a saturating value, matching
/// `arith`'s "integer arithmetic overflow": saturating would only trade a wrap
/// for a different wrong number.
fn temporal_arith(op: BinOp, l: &Value, r: &Value) -> Option<Result<Value>> {
    match (op, l, r) {
        (BinOp::Add, Value::Timestamp(t, u), Value::Interval(i))
        | (BinOp::Add, Value::Interval(i), Value::Timestamp(t, u)) => Some(ts_shift(*t, *u, i, 1)),
        (BinOp::Sub, Value::Timestamp(t, u), Value::Interval(i)) => Some(ts_shift(*t, *u, i, -1)),
        (BinOp::Add, Value::Date(d), Value::Interval(i))
        | (BinOp::Add, Value::Interval(i), Value::Date(d)) => Some(date_shift(*d, i, 1)),
        (BinOp::Sub, Value::Date(d), Value::Interval(i)) => Some(date_shift(*d, i, -1)),
        (BinOp::Sub, Value::Timestamp(a, ua), Value::Timestamp(b, ub)) => {
            Some(ts_diff(*a, *ua, *b, *ub))
        }
        // `DATE - DATE` and `TIME - TIME` are typed as INTERVAL by `infer`, so
        // without these arms they bound cleanly and then died at produce time
        // with "non-numeric operand Date(19723)" — a bind-time-valid expression
        // that could never evaluate.
        (BinOp::Sub, Value::Date(a), Value::Date(b)) => Some(Ok(Value::Interval(Interval {
            months: 0,
            days: (*a as i64 - *b as i64) as i32,
            nanos: 0,
        }))),
        (BinOp::Sub, Value::Time(a, ua), Value::Time(b, ub)) => {
            let nanos = *a as i128 * unit_nanos(*ua) as i128 - *b as i128 * unit_nanos(*ub) as i128;
            // A time of day is under 24h, so the difference always fits i64.
            Some(Ok(Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos: nanos as i64,
            })))
        }
        _ => None,
    }
}

/// `t ± interval`, in the timestamp's own unit.
fn ts_shift(t: i64, u: TimeUnit, i: &Interval, sign: i128) -> Result<Value> {
    let out = t as i128 + sign * interval_in_unit(i, u);
    i64::try_from(out)
        .map(|v| Value::Timestamp(v, u))
        .map_err(|_| MrError::Eval("TIMESTAMP arithmetic is out of range".into()))
}

/// `date ± interval`, in whole days.
fn date_shift(d: i32, i: &Interval, sign: i128) -> Result<Value> {
    let out = d as i128 + sign * interval_in_days(i);
    i32::try_from(out)
        .map(Value::Date)
        .map_err(|_| MrError::Eval("DATE arithmetic is out of range".into()))
}

/// The interval between two timestamps, whatever their units.
///
/// A nanosecond count cannot span the timestamp range: i64 nanoseconds cover
/// ~292 years and timestamps cover ~11,000. So a difference too large for
/// `nanos` carries its whole days in `days` instead, which is **lossless** —
/// every consumer (`interval_nanos`, `interval_in_unit`, `interval_in_days`)
/// treats a day as exactly [`NS_PER_DAY`]. A difference that already fits keeps
/// the exact shape it has always had, so only the inputs that used to panic or
/// wrap change at all.
fn ts_diff(a: i64, ua: TimeUnit, b: i64, ub: TimeUnit) -> Result<Value> {
    let nanos = a as i128 * unit_nanos(ua) as i128 - b as i128 * unit_nanos(ub) as i128;
    if let Ok(n) = i64::try_from(nanos) {
        return Ok(Value::Interval(Interval {
            months: 0,
            days: 0,
            nanos: n,
        }));
    }
    // `%` keeps the dividend's sign and is under a day in magnitude, so both
    // halves carry the same sign and the i64 cast cannot lose anything.
    let days = i32::try_from(nanos / NS_PER_DAY).map_err(|_| {
        MrError::Eval("timestamp difference is out of range for an INTERVAL".into())
    })?;
    Ok(Value::Interval(Interval {
        months: 0,
        days,
        nanos: (nanos % NS_PER_DAY) as i64,
    }))
}

/// An interval as a tick count in `unit`.
///
/// i128 throughout: `months * 30 * 86_400e9` overflows i64 past ~113 months, so
/// `ts + INTERVAL 10 YEAR` was already in range of the bug. `i32::MAX` months is
/// ~5.6e24 ns, comfortably inside i128, so this cannot itself fail — the range
/// check belongs at the use site, where the result meets a real timestamp.
fn interval_in_unit(i: &Interval, unit: TimeUnit) -> i128 {
    // Calendar parts approximated (month = 30 days); exact only for time parts.
    let total_nanos =
        (i.months as i128) * 30 * NS_PER_DAY + (i.days as i128) * NS_PER_DAY + i.nanos as i128;
    total_nanos / unit_nanos(unit) as i128
}

/// An interval as a whole number of days. i128 for the same reason as above:
/// `i.months * 30` was i32 arithmetic and wrapped past ~71.6M months.
fn interval_in_days(i: &Interval) -> i128 {
    (i.months as i128) * 30 + i.days as i128 + i.nanos as i128 / NS_PER_DAY
}

/// Negate a numeric value (3-valued).
pub fn negate(v: &Value) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => Ok(Value::Int(-i)),
        // Negating an unsigned value leaves the unsigned range, so it widens —
        // see the matching rule in `infer`, which types `-u` as HUGEINT so the
        // static type and the runtime value agree. (DuckDB wraps here instead;
        // this is a deliberate divergence, in the same spirit as the documented
        // Flink one.)
        Value::UInt(i) => Ok(Value::HugeInt(-(*i as i128))),
        Value::HugeInt(i) => Ok(Value::HugeInt(-i)),
        Value::Double(d) => Ok(Value::Double(-d)),
        Value::Decimal(u, s) => Ok(Value::Decimal(-u, *s)),
        other => Err(MrError::Eval(format!("cannot negate {other:?}"))),
    }
}

/// A **total** order for sorting, placing NULLs first or last as requested.
///
/// Deliberately not [`compare`], which implements SQL comparison semantics: that
/// maps an unordered pair to `None`, and treating that as `Equal` is intransitive
/// (a NaN key would be "equal" to both 1 and 2 while 1 != 2). `sort_by` with an
/// intransitive comparator is free to return an arbitrary permutation, so a single
/// NaN in an ORDER BY column could scramble the partition. Sorting therefore uses
/// a total order: `total_cmp` for floats, and exact integer/decimal comparison
/// where the values allow it, which also matches what a typed store comparing
/// Arrow buffers in place will do.
pub fn cmp_for_sort(a: &Value, b: &Value, desc: bool, nulls_first: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    // NULL placement is absolute: `nulls_first` says where NULLs land in the final
    // order, so it must be decided here rather than by reversing the whole
    // comparison for DESC — reversing flips the NULLs too, which made
    // `ORDER BY k DESC NULLS FIRST` put them last.
    match (a.is_null(), b.is_null()) {
        (true, true) => return Equal,
        (true, false) => {
            return if nulls_first { Less } else { Greater };
        }
        (false, true) => {
            return if nulls_first { Greater } else { Less };
        }
        (false, false) => {}
    }
    let ord = cmp_present(a, b);
    if desc {
        ord.reverse()
    } else {
        ord
    }
}

/// Total order over two non-NULL values.
fn cmp_present(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    // Exact where possible: `as_f64` would lose precision past 2^53. The
    // integer family goes through the same helper `compare` uses, so the SQL
    // comparison and the sort order cannot disagree about which of two rows is
    // larger — including for a mixed `BIGINT`/`HUGEINT` pair, which these arms
    // used to drop to `as_f64`.
    if let Some(ord) = cmp_ints(a, b) {
        return ord;
    }
    match (a, b) {
        (Value::Decimal(x, sx), Value::Decimal(y, sy)) if sx == sy => x.cmp(y),
        (Value::Double(x), Value::Double(y)) => x.total_cmp(y),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            // Non-numeric (strings, temporals, booleans): SQL comparison is already
            // a total order for a single column's type.
            _ => compare(a, b).unwrap_or(Equal),
        },
    }
}

/// Coerce a value to a target type (CAST and final output coercion).
pub fn coerce(v: Value, ty: &Ty) -> Result<Value> {
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
            Value::UInt(u) => Ok(Value::Int(*u as i64)),
            Value::HugeInt(i) => Ok(Value::Int(*i as i64)),
            Value::Double(d) => Ok(Value::Int(*d as i64)),
            Value::Decimal(u, s) => Ok(Value::Int((*u / pow10(*s)) as i64)),
            Value::Bool(b) => Ok(Value::Int(*b as i64)),
            Value::Str(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| MrError::Eval(format!("cannot cast '{s}' to BIGINT"))),
            _ => Err(MrError::Eval(format!("cannot cast {v:?} to BIGINT"))),
        },
        // A negative value is not a UBIGINT, and `as u64` on one is
        // 18446744073709551615 — the exact corruption this type exists to
        // remove — so it is an error, as it is in DuckDB. The `Str` arm is the
        // escape hatch for a literal above i64::MAX, which the expression lexer
        // cannot tokenize: `CAST('18446744073709551615' AS UBIGINT)`.
        Ty::UInt64 => match &v {
            Value::UInt(_) => Ok(v),
            Value::Int(i) => u64::try_from(*i)
                .map(Value::UInt)
                .map_err(|_| MrError::Eval(format!("{i} is out of range for UBIGINT"))),
            Value::HugeInt(i) => u64::try_from(*i)
                .map(Value::UInt)
                .map_err(|_| MrError::Eval(format!("{i} is out of range for UBIGINT"))),
            Value::Double(d) if *d >= 0.0 && *d <= u64::MAX as f64 => Ok(Value::UInt(*d as u64)),
            Value::Bool(b) => Ok(Value::UInt(*b as u64)),
            Value::Str(s) => s
                .trim()
                .parse::<u64>()
                .map(Value::UInt)
                .map_err(|_| MrError::Eval(format!("cannot cast '{s}' to UBIGINT"))),
            _ => Err(MrError::Eval(format!("cannot cast {v:?} to UBIGINT"))),
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
            let unscaled = (f * 10f64.powi(*s as i32)).round() as i128;
            Ok(Value::Decimal(unscaled, *s))
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
        // A list coerces element-wise; a scalar is not a list.
        Ty::List(elem) => match v {
            Value::List(items) => Ok(Value::List(
                items
                    .into_iter()
                    .map(|it| coerce(it, elem))
                    .collect::<Result<Vec<_>>>()?,
            )),
            other => Err(MrError::Eval(format!("cannot cast {other:?} to a LIST"))),
        },
        Ty::Null => Ok(Value::Null),
    }
}
