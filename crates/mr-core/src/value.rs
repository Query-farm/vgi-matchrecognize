//! The core's dynamic typed `Value` — the runtime counterpart of [`Ty`](crate::types::Ty).
//!
//! [`Value`] is what a [`RowStore`](crate::engine::RowStore) hands back for a
//! cell and what the evaluator threads through expression evaluation. It carries
//! enough type information that the evaluator can do 3-valued logic, numeric
//! promotion, and temporal arithmetic without consulting the schema.

use crate::types::TimeUnit;

/// An interval as a `(months, days, nanos)` triple (Arrow `MonthDayNano`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub nanos: i64,
}

/// A dynamic typed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL (the unknown). 3-valued logic applies.
    Null,
    Bool(bool),
    /// Integer family (any width), widened to i64.
    Int(i64),
    /// 128-bit integer (HUGEINT, e.g. integer SUM).
    HugeInt(i128),
    Double(f64),
    /// Fixed-scale decimal: unscaled value + scale.
    Decimal(i128, i8),
    Str(String),
    /// Days since the Unix epoch (Arrow `Date32`).
    Date(i32),
    /// Ticks since the Unix epoch at the given resolution.
    Timestamp(i64, TimeUnit),
    /// Time-of-day ticks at the given resolution.
    Time(i64, TimeUnit),
    Interval(Interval),
}

impl Value {
    /// Whether this value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Interpret as a boolean for predicate purposes: `Some(b)` for a real
    /// boolean, `None` for NULL (unknown). Non-boolean values are an error
    /// caught earlier by inference, but we coerce numeric 0/non-0 defensively.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Null => None,
            Value::Int(v) => Some(*v != 0),
            _ => None,
        }
    }

    /// Numeric value as f64, if this value is numeric (else `None`).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::HugeInt(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            Value::Decimal(u, s) => Some(*u as f64 / 10f64.powi(*s as i32)),
            _ => None,
        }
    }

    /// Integer value as i128, if this value is an integer family.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Int(v) => Some(*v as i128),
            Value::HugeInt(v) => Some(*v),
            _ => None,
        }
    }
}
