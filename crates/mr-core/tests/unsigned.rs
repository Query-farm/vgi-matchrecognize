//! `UBIGINT` — values above `i64::MAX` must survive as keys, predicates,
//! measures and aggregates.
//!
//! They did not: `arrow_to_ty` folded `UInt64` into `Ty::Int64` and `cell_value`
//! did `as i64`, so anything above `i64::MAX` arrived negative and stayed that
//! way through comparison, ordering and output alike. The fix is a real
//! `Ty::UInt64`/`Value::UInt` pair rather than a wider signed type, because
//! `u64` and `i64` contain neither the other.
//!
//! The values below are chosen to straddle 2^63 deliberately — that is the only
//! region where the old and new readings differ, and `u64::MAX` and
//! `u64::MAX - 1` are additionally the *same* f64, so they also catch any
//! surviving `as_f64` fallback.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::error::MrError;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch {
    cols: HashMap<String, Ty>,
    labels: Vec<String>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        self.labels.iter().any(|v| v == name)
    }
}

fn sch() -> Sch {
    Sch {
        cols: [
            ("id".to_string(), Ty::Int64),
            ("u".to_string(), Ty::UInt64),
            ("v".to_string(), Ty::Int64),
        ]
        .into_iter()
        .collect(),
        labels: vec!["A".to_string()],
    }
}

/// Rows of `(id BIGINT, u UBIGINT, v BIGINT)`.
fn store(rows: &[(i64, u64, i64)]) -> VecRowStore {
    VecRowStore::new(
        vec![("id", Ty::Int64), ("u", Ty::UInt64), ("v", Ty::Int64)],
        rows.iter()
            .map(|(id, u, v)| vec![Value::Int(*id), Value::UInt(*u), Value::Int(*v)])
            .collect(),
    )
}

struct Q<'a> {
    pattern: &'a str,
    define: &'a str,
    measures: &'a str,
    partition_by: Vec<String>,
    order_by: Vec<String>,
    after: &'a str,
}

impl Default for Q<'_> {
    fn default() -> Self {
        Q {
            pattern: "A+",
            define: "{}",
            measures: r#"{"m":"LAST(u)"}"#,
            partition_by: vec![],
            order_by: vec!["id".to_string()],
            after: "past last row",
        }
    }
}

fn plan_of(q: &Q) -> mr_core::error::Result<Plan> {
    Plan::build(
        &PlanConfig {
            include: Vec::new(),
            pattern: q.pattern.into(),
            define_json: q.define.into(),
            subset_json: String::new(),
            measures_json: Some(q.measures.into()),
            partition_by: q.partition_by.clone(),
            order_by: q.order_by.clone(),
            rows_all: false,
            omit_empty_matches: false,
            after: q.after.into(),
            step_budget: Some(10_000_000),
        },
        &sch(),
    )
}

fn run(q: Q, rows: &[(i64, u64, i64)]) -> Vec<Vec<Value>> {
    try_run(q, rows).unwrap()
}

fn try_run(q: Q, rows: &[(i64, u64, i64)]) -> mr_core::error::Result<Vec<Vec<Value>>> {
    let plan = plan_of(&q)?;
    plan.run(&store(rows))
}

// --- the headline round trip -----------------------------------------------

/// `u64::MAX` must come back as itself, in a `UBIGINT` column. It used to come
/// back as `-1` in a `BIGINT` one.
#[test]
fn u64_max_round_trips() {
    let plan = plan_of(&Q::default()).unwrap();
    assert_eq!(plan.output_columns()[0].ty, Ty::UInt64);
    let out = run(Q::default(), &[(1, u64::MAX, 0)]);
    // Columns: m
    assert_eq!(out[0][0], Value::UInt(u64::MAX));
}

/// `u64::MAX` and `u64::MAX - 1` are the same f64, so this is the single test
/// that fails if any `as_f64` comparison fallback survives.
#[test]
fn adjacent_u64_values_are_distinguished() {
    let ascending = run(
        Q {
            define: r#"{"A":"u > PREV(u) OR PREV(u) IS NULL"}"#,
            measures: r#"{"n":"COUNT(*)"}"#,
            ..Q::default()
        },
        &[(1, u64::MAX - 1, 0), (2, u64::MAX, 0)],
    );
    // Columns: n — both rows bind, so the run is length 2.
    assert_eq!(ascending[0][0], Value::Int(2));

    // Descending: the second row must not satisfy `u > PREV(u)`.
    let descending = run(
        Q {
            define: r#"{"A":"u > PREV(u) OR PREV(u) IS NULL"}"#,
            measures: r#"{"n":"COUNT(*)"}"#,
            ..Q::default()
        },
        &[(1, u64::MAX, 0), (2, u64::MAX - 1, 0)],
    );
    assert_eq!(descending[0][0], Value::Int(1));
}

/// A `u64` above `i64::MAX` compared against a negative `i64`: neither type
/// contains the other, and i128 is what makes the answer exact.
#[test]
fn mixed_sign_comparison() {
    let out = run(
        Q {
            pattern: "A",
            define: r#"{"A":"u > v"}"#,
            measures: r#"{"id":"LAST(id)"}"#,
            after: "to next row",
            ..Q::default()
        },
        &[(1, u64::MAX, -1), (2, 0, 5)],
    );
    // Columns: id — only row 1 satisfies u > v.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(1));
}

// --- keys ------------------------------------------------------------------

/// Two keys that are one apart at the top of the range are two partitions, not
/// one. As f64 they are indistinguishable.
#[test]
fn ubigint_partition_key_groups_exactly() {
    let out = run(
        Q {
            partition_by: vec!["u".to_string()],
            measures: r#"{"n":"COUNT(*)"}"#,
            ..Q::default()
        },
        &[(1, u64::MAX, 0), (2, u64::MAX - 1, 0), (3, u64::MAX, 0)],
    );
    // Columns: u, n
    assert_eq!(out.len(), 2, "two distinct keys => two partitions");
    assert_eq!(out[0][0], Value::UInt(u64::MAX));
    assert_eq!(out[0][1], Value::Int(2));
    assert_eq!(out[1][0], Value::UInt(u64::MAX - 1));
    assert_eq!(out[1][1], Value::Int(1));
}

/// Values straddling 2^63, shuffled, sorted as `u64` — under an `i64` reading
/// the upper half is negative and sorts first.
///
/// `MIN_ROWS_FOR_KEY_SORT` is 256, so this exercises the *packed* path
/// (`KeyCol::UInts`).
#[test]
fn ubigint_order_key_packed_path() {
    let n = 400i64;
    let rows: Vec<(i64, u64, i64)> = (0..n)
        .map(|i| {
            // Deterministic shuffle, straddling 2^63.
            let k = (i * 167 % n) as u64;
            (
                i,
                (1u64 << 63).wrapping_add(k).wrapping_sub(n as u64 / 2),
                0,
            )
        })
        .collect();
    let out = run(
        Q {
            order_by: vec!["u".to_string()],
            measures: r#"{"first":"FIRST(u)","last":"LAST(u)"}"#,
            ..Q::default()
        },
        &rows,
    );
    let mut sorted: Vec<u64> = rows.iter().map(|r| r.1).collect();
    sorted.sort_unstable();
    // Columns: first, last
    assert_eq!(out[0][0], Value::UInt(sorted[0]));
    assert_eq!(out[0][1], Value::UInt(*sorted.last().unwrap()));
}

/// The same values below the packed-sort threshold, so `sort_tape` takes
/// `cmp_cells` -> `cmp_for_sort` instead. The two paths must agree.
#[test]
fn ubigint_order_key_fallback_path() {
    let rows: Vec<(i64, u64, i64)> = [
        (1, u64::MAX, 0),
        (2, 0, 0),
        (3, 1u64 << 63, 0),
        (4, (1u64 << 63) - 1, 0),
    ]
    .into_iter()
    .collect();
    let out = run(
        Q {
            order_by: vec!["u".to_string()],
            measures: r#"{"first":"FIRST(u)","last":"LAST(u)"}"#,
            ..Q::default()
        },
        &rows,
    );
    // Columns: first, last
    assert_eq!(out[0][0], Value::UInt(0));
    assert_eq!(out[0][1], Value::UInt(u64::MAX));
}

// --- aggregates and arithmetic ---------------------------------------------

/// `SUM(UBIGINT)` widens to HUGEINT, so `u64::MAX + 1` is representable rather
/// than an overflow.
#[test]
fn sum_over_ubigint_is_hugeint() {
    let q = Q {
        measures: r#"{"s":"SUM(u)"}"#,
        ..Q::default()
    };
    assert_eq!(plan_of(&q).unwrap().output_columns()[0].ty, Ty::HugeInt);
    let out = run(q, &[(1, u64::MAX, 0), (2, 1, 0)]);
    // Columns: s
    assert_eq!(out[0][0], Value::HugeInt(u64::MAX as i128 + 1));
}

#[test]
fn avg_over_ubigint_is_double() {
    let q = Q {
        measures: r#"{"a":"AVG(u)"}"#,
        ..Q::default()
    };
    assert_eq!(plan_of(&q).unwrap().output_columns()[0].ty, Ty::Double);
}

/// `MIN`/`MAX` keep the argument type and must pick the true extreme, not the
/// first of several values that merely looked equal as floats.
#[test]
fn min_max_over_ubigint_stay_unsigned_and_exact() {
    let q = Q {
        measures: r#"{"lo":"MIN(u)","hi":"MAX(u)"}"#,
        ..Q::default()
    };
    assert_eq!(plan_of(&q).unwrap().output_columns()[0].ty, Ty::UInt64);
    let out = run(q, &[(1, u64::MAX - 1, 0), (2, u64::MAX, 0), (3, 7, 0)]);
    // Columns: lo, hi
    assert_eq!(out[0][0], Value::UInt(7));
    assert_eq!(out[0][1], Value::UInt(u64::MAX));
}

/// Negating an unsigned value leaves the unsigned range, so `-u` is HUGEINT.
/// Typing it UBIGINT would bind fine and then fail at `coerce` on every
/// negative result.
#[test]
fn negate_ubigint_widens_to_hugeint() {
    let q = Q {
        measures: r#"{"n":"-LAST(u)"}"#,
        ..Q::default()
    };
    assert_eq!(plan_of(&q).unwrap().output_columns()[0].ty, Ty::HugeInt);
    let out = run(q, &[(1, 1, 0)]);
    // Columns: n — -1, not 18446744073709551615.
    assert_eq!(out[0][0], Value::HugeInt(-1));
}

/// Mixing signed and unsigned widens to HUGEINT, which is what keeps the
/// mixed-sign case from ever reaching a runtime range check.
#[test]
fn ubigint_plus_bigint_is_hugeint() {
    let q = Q {
        measures: r#"{"s":"LAST(u) + LAST(v)"}"#,
        ..Q::default()
    };
    assert_eq!(plan_of(&q).unwrap().output_columns()[0].ty, Ty::HugeInt);
    let out = run(q, &[(1, u64::MAX, -1)]);
    // Columns: s
    assert_eq!(out[0][0], Value::HugeInt(u64::MAX as i128 - 1));
}

/// An underflowing `u - u` is typed UBIGINT (both sides agree, as in DuckDB)
/// and so is a clean range error rather than 18446744073709551615.
#[test]
fn ubigint_underflow_is_an_error() {
    let err = try_run(
        Q {
            measures: r#"{"d":"FIRST(u) - LAST(u)"}"#,
            ..Q::default()
        },
        &[(1, 1, 0), (2, 2, 0)],
    )
    .unwrap_err();
    assert!(
        matches!(err, MrError::Eval(ref m) if m.contains("UBIGINT")),
        "expected a clean range error, got {err:?}"
    );
}

// --- rendering and casts ---------------------------------------------------

/// A UBIGINT in a VARCHAR column must render as digits, not as `UInt(42)`.
#[test]
fn ubigint_to_varchar() {
    let out = run(
        Q {
            measures: r#"[{"as":"s","expr":"LAST(u)","type":"VARCHAR"}]"#,
            ..Q::default()
        },
        &[(1, u64::MAX, 0)],
    );
    // Columns: s
    assert_eq!(out[0][0], Value::Str("18446744073709551615".to_string()));
}

/// The expression lexer parses digit runs as i64, so a literal above `i64::MAX`
/// cannot be written directly. Casting from a string is the documented way in.
#[test]
fn cast_from_string_to_ubigint() {
    let out = run(
        Q {
            measures: r#"{"u":"CAST('18446744073709551615' AS UBIGINT)"}"#,
            ..Q::default()
        },
        &[(1, 0, 0)],
    );
    // Columns: u
    assert_eq!(out[0][0], Value::UInt(u64::MAX));
}

/// ... and the limitation it works around is real, so pin it: a bare literal
/// above i64::MAX is a lex error, not a silently truncated value.
#[test]
fn a_literal_above_i64_max_is_refused() {
    let err = try_run(
        Q {
            measures: r#"{"x":"18446744073709551615"}"#,
            ..Q::default()
        },
        &[(1, 0, 0)],
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("out of range"),
        "expected a lex range error, got {err}"
    );
}

/// Casting a negative to UBIGINT is an error, not a wrap.
#[test]
fn casting_a_negative_to_ubigint_is_an_error() {
    let err = try_run(
        Q {
            measures: r#"{"u":"CAST(LAST(v) AS UBIGINT)"}"#,
            ..Q::default()
        },
        &[(1, 0, -1)],
    )
    .unwrap_err();
    assert!(
        matches!(err, MrError::Eval(ref m) if m.contains("UBIGINT")),
        "expected a range error, got {err:?}"
    );
}

/// Scalar functions keep the unsigned type and the exact value.
#[test]
fn scalar_functions_over_ubigint() {
    let out = run(
        Q {
            measures: r#"{"a":"abs(LAST(u))","r":"round(LAST(u))","g":"greatest(LAST(u), FIRST(u))"}"#,
            ..Q::default()
        },
        &[(1, u64::MAX - 1, 0), (2, u64::MAX, 0)],
    );
    // Columns: a, r, g
    assert_eq!(out[0][0], Value::UInt(u64::MAX));
    assert_eq!(out[0][1], Value::UInt(u64::MAX));
    assert_eq!(out[0][2], Value::UInt(u64::MAX));
}
