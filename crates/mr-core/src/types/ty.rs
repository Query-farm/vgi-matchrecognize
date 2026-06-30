//! The `Ty` enum — the core's static type system, in 1:1 correspondence with
//! the Arrow `DataType`s the worker emits in the output schema.
//!
//! Keeping this Arrow-free lets the whole inference pass be unit-tested with a
//! plain `name -> Ty` map and no Arrow/VGI machinery.

/// Time resolution for temporal types (mirrors Arrow's `TimeUnit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    Second,
    Milli,
    Micro,
    Nano,
}

/// A static type. Each variant maps to exactly one Arrow `DataType`:
///
/// | `Ty`              | Arrow `DataType`            | DuckDB type      |
/// |-------------------|-----------------------------|------------------|
/// | `Boolean`         | `Boolean`                   | `BOOLEAN`        |
/// | `Int64`           | `Int64`                     | `BIGINT`         |
/// | `HugeInt`         | `Decimal128(38, 0)`         | `HUGEINT`/`DECIMAL`|
/// | `Double`          | `Float64`                   | `DOUBLE`         |
/// | `Decimal(p, s)`   | `Decimal128(p, s)`          | `DECIMAL(p,s)`   |
/// | `Varchar`         | `Utf8`                      | `VARCHAR`        |
/// | `Date`            | `Date32`                    | `DATE`           |
/// | `Timestamp(u)`    | `Timestamp(u, None)`        | `TIMESTAMP`      |
/// | `Time(u)`         | `Time64(u)`                 | `TIME`           |
/// | `Interval`        | `Interval(MonthDayNano)`    | `INTERVAL`       |
/// | `Null`            | (no concrete output type)   | —                |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    Boolean,
    Int64,
    HugeInt,
    Double,
    Decimal(u8, i8),
    Varchar,
    Date,
    Timestamp(TimeUnit),
    Time(TimeUnit),
    Interval,
    /// The "unknown" type — produced by a bare `NULL` literal or a wholly-NULL
    /// column. Unifies with anything; a measure that resolves to `Null` with no
    /// `type` override is a bind error.
    Null,
}

impl Ty {
    /// Whether this is an integer-family type (`BIGINT`).
    pub fn is_integer(self) -> bool {
        matches!(self, Ty::Int64)
    }

    /// Whether this is any numeric type (integer / hugeint / double / decimal).
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Ty::Int64 | Ty::HugeInt | Ty::Double | Ty::Decimal(_, _)
        )
    }

    /// Whether this is a temporal type (date / timestamp / time).
    pub fn is_temporal(self) -> bool {
        matches!(self, Ty::Date | Ty::Timestamp(_) | Ty::Time(_))
    }

    /// Rank in the numeric promotion lattice: `Int64 < HugeInt < Double`.
    /// `Decimal` is handled separately (never silently joins binary floats).
    fn numeric_rank(self) -> Option<u8> {
        match self {
            Ty::Int64 => Some(0),
            Ty::HugeInt => Some(1),
            Ty::Double => Some(2),
            _ => None,
        }
    }

    /// Least-upper-bound (join) of two types under the promotion lattice, or
    /// `None` when they do not unify (a bind error at the call site).
    pub fn unify(self, other: Ty) -> Option<Ty> {
        use Ty::*;
        // NULL unifies with anything (yields the other type).
        match (self, other) {
            (Null, t) | (t, Null) => return Some(t),
            _ => {}
        }
        if self == other {
            return Some(self);
        }
        // Decimal arithmetic widening: keep the larger precision/scale.
        if let (Decimal(p1, s1), Decimal(p2, s2)) = (self, other) {
            return Some(Decimal(p1.max(p2), s1.max(s2)));
        }
        // A decimal combined with a binary float widens to DOUBLE.
        if (matches!(self, Decimal(_, _)) && matches!(other, Double))
            || (matches!(other, Decimal(_, _)) && matches!(self, Double))
        {
            return Some(Double);
        }
        // A decimal combined with an integer family stays decimal (give it
        // headroom for the integer part).
        if let (Decimal(p, s), o) | (o, Decimal(p, s)) = (self, other) {
            if matches!(o, Int64 | HugeInt) {
                return Some(Decimal(p.max(20), s));
            }
        }
        // Pure numeric lattice.
        if let (Some(a), Some(b)) = (self.numeric_rank(), other.numeric_rank()) {
            return Some(if a >= b { self } else { other });
        }
        // Temporal types unify with themselves only (handled by `self == other`).
        None
    }

    /// SUM widening: integer family -> HUGEINT; float -> DOUBLE; decimal ->
    /// `DECIMAL(38, s)`.
    pub fn sum_ty(self) -> Option<Ty> {
        match self {
            Ty::Int64 | Ty::HugeInt => Some(Ty::HugeInt),
            Ty::Double => Some(Ty::Double),
            Ty::Decimal(_, s) => Some(Ty::Decimal(38, s)),
            _ => None,
        }
    }

    /// AVG widening: numeric -> DOUBLE (DECIMAL -> `DECIMAL(38, max(s,4))`).
    pub fn avg_ty(self) -> Option<Ty> {
        match self {
            Ty::Int64 | Ty::HugeInt | Ty::Double => Some(Ty::Double),
            Ty::Decimal(_, s) => Some(Ty::Decimal(38, s.max(4))),
            _ => None,
        }
    }
}
