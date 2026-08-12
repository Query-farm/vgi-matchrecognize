//! The `Ty` enum — the core's static type system, in 1:1 correspondence with
//! the Arrow `DataType`s the worker emits in the output schema.
//!
//! Keeping this Arrow-free lets the whole inference pass be unit-tested with a
//! plain `name -> Ty` map and no Arrow/VGI machinery.

use std::fmt;

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
/// | `UInt64`          | `UInt64`                    | `UBIGINT`        |
/// | `HugeInt`         | `Decimal128(38, 0)`         | `HUGEINT`/`DECIMAL`|
/// | `Double`          | `Float64`                   | `DOUBLE`         |
/// | `Decimal(p, s)`   | `Decimal128(p, s)`          | `DECIMAL(p,s)`   |
/// | `Varchar`         | `Utf8`                      | `VARCHAR`        |
/// | `Date`            | `Date32`                    | `DATE`           |
/// | `Timestamp(u)`    | `Timestamp(u, None)`        | `TIMESTAMP`      |
/// | `Time(u)`         | `Time64(u)`                 | `TIME`           |
/// | `Interval`        | `Interval(MonthDayNano)`    | `INTERVAL`       |
/// | `List(elem)`      | `List(Field<elem>)`         | `elem[]`         |
/// | `Null`            | (no concrete output type)   | —                |
///
/// Not `Copy`: `List` owns its element type. Everything else is a scalar, so
/// clones are cheap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    Boolean,
    Int64,
    /// `UBIGINT`. Its own variant rather than a widening to `Int64`, because a
    /// `u64` above `i64::MAX` has no `i64` representation — casting one is the
    /// corruption this exists to prevent. The narrower unsigned widths
    /// (`UTINYINT`..`UINTEGER`) all fit `i64` exactly and stay `Int64`.
    UInt64,
    HugeInt,
    Double,
    Decimal(u8, i8),
    Varchar,
    Date,
    Timestamp(TimeUnit),
    Time(TimeUnit),
    Interval,
    /// `LIST(elem)` — produced by `array_agg`.
    List(Box<Ty>),
    /// The "unknown" type — produced by a bare `NULL` literal or a wholly-NULL
    /// column. Unifies with anything; a measure that resolves to `Null` with no
    /// `type` override is a bind error.
    Null,
}

/// The DuckDB spelling of this type — what a user would write in a `CAST`, and
/// what the `type` override on a measure accepts.
///
/// Type names reach users through every inference error ("cannot compare
/// VARCHAR with BIGINT"), so they must be SQL, not the Rust variant names:
/// `Varchar`/`Int64` name our AST rather than anything the reader can write.
/// The spellings here match the `Ty` -> DuckDB column of the table above, and
/// round-trip through `expr::parser::parse_type_name`.
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Boolean => f.write_str("BOOLEAN"),
            Ty::Int64 => f.write_str("BIGINT"),
            Ty::UInt64 => f.write_str("UBIGINT"),
            Ty::HugeInt => f.write_str("HUGEINT"),
            Ty::Double => f.write_str("DOUBLE"),
            Ty::Decimal(p, s) => write!(f, "DECIMAL({p},{s})"),
            Ty::Varchar => f.write_str("VARCHAR"),
            Ty::Date => f.write_str("DATE"),
            // DuckDB spells the non-microsecond resolutions with a suffix, and
            // plain `TIMESTAMP` for microseconds.
            Ty::Timestamp(u) => match u {
                TimeUnit::Second => f.write_str("TIMESTAMP_S"),
                TimeUnit::Milli => f.write_str("TIMESTAMP_MS"),
                TimeUnit::Micro => f.write_str("TIMESTAMP"),
                TimeUnit::Nano => f.write_str("TIMESTAMP_NS"),
            },
            // TIME has one spelling in DuckDB whatever the underlying unit.
            Ty::Time(_) => f.write_str("TIME"),
            Ty::Interval => f.write_str("INTERVAL"),
            Ty::List(elem) => write!(f, "{elem}[]"),
            Ty::Null => f.write_str("NULL"),
        }
    }
}

impl Ty {
    /// Whether this is an integer-family type (`BIGINT` / `UBIGINT`).
    pub fn is_integer(&self) -> bool {
        matches!(self, Ty::Int64 | Ty::UInt64)
    }

    /// Whether this is any numeric type (integer / hugeint / double / decimal).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Ty::Int64 | Ty::UInt64 | Ty::HugeInt | Ty::Double | Ty::Decimal(_, _)
        )
    }

    /// Whether this is a temporal type (date / timestamp / time).
    pub fn is_temporal(&self) -> bool {
        matches!(self, Ty::Date | Ty::Timestamp(_) | Ty::Time(_))
    }

    /// Rank in the numeric promotion lattice: `Int64 < HugeInt < Double`.
    /// `Decimal` is handled separately (never silently joins binary floats).
    ///
    /// **`UInt64` deliberately has no rank.** A rank is a claim about
    /// *containment* — `unify` takes whichever side ranks higher — and `u64` and
    /// `i64` contain neither the other: `u64::MAX` is not an `i64` and `-1` is
    /// not a `u64`. Every value one could give it is wrong in a different way:
    /// sharing rank 0 makes `unify` depend on argument order, so `u + v` and
    /// `v + u` would type differently; ranking it above or below `Int64`
    /// reintroduces the wrapping this type exists to prevent, one level up.
    ///
    /// So `UInt64`'s joins are spelled out explicitly in [`Ty::unify`] *before*
    /// the rank fallback, and leaving it `None` here is the safety net: a pair
    /// that reaches this fallback with a `UInt64` in it yields `None`, i.e. a
    /// clean bind error rather than a silently wrong type. Do not "tidy" this by
    /// giving it a rank.
    fn numeric_rank(&self) -> Option<u8> {
        match self {
            Ty::Int64 => Some(0),
            Ty::HugeInt => Some(1),
            Ty::Double => Some(2),
            _ => None,
        }
    }

    /// Least-upper-bound (join) of two types under the promotion lattice, or
    /// `None` when they do not unify (a bind error at the call site).
    pub fn unify(&self, other: &Ty) -> Option<Ty> {
        use Ty::*;
        // NULL unifies with anything (yields the other type).
        match (self, other) {
            (Null, t) | (t, Null) => return Some(t.clone()),
            _ => {}
        }
        if self == other {
            return Some(self.clone());
        }
        // Lists unify element-wise.
        if let (List(a), List(b)) = (self, other) {
            return a.unify(b).map(|e| List(Box::new(e)));
        }
        // Decimal arithmetic widening: keep the larger precision/scale.
        if let (Decimal(p1, s1), Decimal(p2, s2)) = (self, other) {
            return Some(Decimal(*p1.max(p2), *s1.max(s2)));
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
            if matches!(o, Int64 | UInt64 | HugeInt) {
                return Some(Decimal((*p).max(20), *s));
            }
        }
        // UBIGINT's joins, before the rank fallback — it has no rank, and see
        // `numeric_rank` for why it must not be given one. The or-pattern makes
        // these commutative by construction, the same shape the Decimal arm
        // above uses.
        if let (UInt64, o) | (o, UInt64) = (self, other) {
            return match o {
                // i128 holds every i64 and every u64 exactly, so it is the only
                // lossless join of the two. (DuckDB agrees: UBIGINT + BIGINT is
                // HUGEINT.)
                Int64 | HugeInt => Some(HugeInt),
                // Lossy past 2^53, exactly as `Int64` joined with `Double`
                // already is.
                Double => Some(Double),
                // `UInt64 ⊔ UInt64` is caught by the `self == other` arm above;
                // anything else does not unify.
                _ => None,
            };
        }
        // Pure numeric lattice.
        if let (Some(a), Some(b)) = (self.numeric_rank(), other.numeric_rank()) {
            return Some(if a >= b { self.clone() } else { other.clone() });
        }
        // Temporal types unify with themselves only (handled by `self == other`).
        None
    }

    /// SUM widening: integer family -> HUGEINT; float -> DOUBLE; decimal ->
    /// `DECIMAL(38, s)`.
    pub fn sum_ty(&self) -> Option<Ty> {
        match self {
            // UBIGINT sums into HUGEINT like BIGINT does: i128 holds any
            // realistic number of u64s, and it is what DuckDB returns.
            Ty::Int64 | Ty::UInt64 | Ty::HugeInt => Some(Ty::HugeInt),
            Ty::Double => Some(Ty::Double),
            Ty::Decimal(_, s) => Some(Ty::Decimal(38, *s)),
            _ => None,
        }
    }

    /// AVG widening: numeric -> DOUBLE (DECIMAL -> `DECIMAL(38, max(s,4))`).
    pub fn avg_ty(&self) -> Option<Ty> {
        match self {
            Ty::Int64 | Ty::UInt64 | Ty::HugeInt | Ty::Double => Some(Ty::Double),
            Ty::Decimal(_, s) => Some(Ty::Decimal(38, (*s).max(4))),
            _ => None,
        }
    }
}
