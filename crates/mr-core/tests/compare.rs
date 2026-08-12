//! `valops::compare` — SQL comparison semantics, and its agreement with the
//! sort order.
//!
//! This is the comparator behind every DEFINE predicate, `IN`, `BETWEEN`,
//! `MIN`/`MAX`, `NULLIF` and `GREATEST`/`LEAST`, and until this file existed
//! nothing imported it. It was tested only end to end, through queries whose
//! values were small enough that the bugs below could not show.
//!
//! Two things were wrong, both because everything numeric fell through to
//! `as_f64`: integers past 2^53 compared equal to their neighbours, and NaN
//! compared *equal to everything*. The sort comparator had already been fixed
//! for both hazards, so the two disagreed — which is why the last test here is
//! an agreement property rather than another example.

use mr_core::engine::valops::{cmp_for_sort, compare};
use mr_core::engine::VecRowStore;
use mr_core::expr::ast::BinOp;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, TimeUnit, Ty};
use mr_core::value::{Interval, Value};
use proptest::prelude::*;
use std::cmp::Ordering;

/// `a <op> b` as SQL sees it: `None` where the result is SQL NULL.
fn sql(op: BinOp, a: &Value, b: &Value) -> Option<bool> {
    match mr_core::engine::valops::binary(op, a, b).unwrap() {
        Value::Bool(b) => Some(b),
        Value::Null => None,
        other => panic!("expected a boolean or NULL, got {other:?}"),
    }
}

// --- exact integers --------------------------------------------------------

/// `f64` has a 53-bit mantissa, so 2^53 and 2^53+1 are the *same* float.
/// Comparing through it made them equal.
#[test]
fn adjacent_i64_values_past_2_53_are_distinguished() {
    let lo = Value::Int(9_007_199_254_740_992); // 2^53
    let hi = Value::Int(9_007_199_254_740_993); // 2^53 + 1
    assert_eq!(compare(&hi, &lo), Some(Ordering::Greater));
    assert_eq!(sql(BinOp::Eq, &hi, &lo), Some(false));
    assert_eq!(sql(BinOp::Gt, &hi, &lo), Some(true));
    assert_eq!(sql(BinOp::Ne, &hi, &lo), Some(true));
}

/// Nanosecond epochs carried as BIGINT sit around 1.7e18, where the f64 spacing
/// is 256 — so a whole range of real timestamps compared equal.
#[test]
fn nanosecond_scale_integers_are_distinguished() {
    let a = Value::Int(1_700_000_000_000_000_000);
    let b = Value::Int(1_700_000_000_000_000_001);
    assert_eq!(compare(&b, &a), Some(Ordering::Greater));
    assert_eq!(sql(BinOp::Gt, &b, &a), Some(true));
}

/// A mixed `BIGINT`/`HUGEINT` pair must be exact too — that combination used to
/// miss even the sort comparator's typed arms and fall to `as_f64`.
#[test]
fn mixed_integer_families_compare_exactly() {
    let big = Value::Int(9_007_199_254_740_993);
    let huge = Value::HugeInt(9_007_199_254_740_992);
    assert_eq!(compare(&big, &huge), Some(Ordering::Greater));
    assert_eq!(compare(&huge, &big), Some(Ordering::Less));
    assert_eq!(
        compare(&Value::HugeInt(1), &Value::Int(1)),
        Some(Ordering::Equal)
    );
}

#[test]
fn i64_extremes_compare_exactly() {
    let min = Value::Int(i64::MIN);
    let max = Value::Int(i64::MAX);
    assert_eq!(compare(&min, &max), Some(Ordering::Less));
    assert_eq!(
        compare(&max, &Value::Int(i64::MAX - 1)),
        Some(Ordering::Greater)
    );
}

// --- NaN is unordered, not equal -------------------------------------------

/// SQL comparison against an unknown is unknown. Reporting NaN as `Equal` made
/// `NaN = 1.0` true and `NaN <> 1.0` false — both wrong, and inconsistent with
/// each other.
#[test]
fn nan_is_unordered_not_equal() {
    let nan = Value::Double(f64::NAN);
    let one = Value::Double(1.0);
    assert_eq!(compare(&nan, &one), None);
    for op in [
        BinOp::Eq,
        BinOp::Ne,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Gt,
        BinOp::Ge,
    ] {
        assert_eq!(sql(op, &nan, &one), None, "NaN {op:?} 1.0 should be NULL");
    }
}

#[test]
fn nan_compared_with_itself_is_still_unordered() {
    let nan = Value::Double(f64::NAN);
    assert_eq!(compare(&nan, &nan.clone()), None);
    assert_eq!(sql(BinOp::Eq, &nan, &nan.clone()), None);
}

/// Infinities are ordinary values — they order, unlike NaN.
#[test]
fn infinities_are_ordered() {
    let inf = Value::Double(f64::INFINITY);
    let neg = Value::Double(f64::NEG_INFINITY);
    assert_eq!(compare(&neg, &inf), Some(Ordering::Less));
    assert_eq!(sql(BinOp::Lt, &neg, &Value::Double(0.0)), Some(true));
}

// --- unordered vs. incomparable --------------------------------------------

#[test]
fn null_is_unordered_against_everything() {
    assert_eq!(compare(&Value::Null, &Value::Int(1)), None);
    assert_eq!(compare(&Value::Int(1), &Value::Null), None);
    assert_eq!(compare(&Value::Null, &Value::Null), None);
}

#[test]
fn incomparable_types_are_unordered() {
    assert_eq!(compare(&Value::Int(1), &Value::Str("1".into())), None);
    assert_eq!(
        compare(&Value::Bool(true), &Value::Str("true".into())),
        None
    );
}

/// The arms that were already exact must stay exact.
#[test]
fn typed_arms_are_unchanged() {
    assert_eq!(
        compare(&Value::Str("a".into()), &Value::Str("b".into())),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare(&Value::Bool(false), &Value::Bool(true)),
        Some(Ordering::Less)
    );
    assert_eq!(
        compare(&Value::Date(1), &Value::Date(2)),
        Some(Ordering::Less)
    );
    // Cross-unit temporals rescale in i128; year 9999 in micros must not wrap.
    assert_eq!(
        compare(
            &Value::Timestamp(253_402_300_799_000_000, TimeUnit::Micro),
            &Value::Timestamp(0, TimeUnit::Nano),
        ),
        Some(Ordering::Greater)
    );
    assert_eq!(
        compare(
            &Value::Interval(Interval {
                months: 0,
                days: 1,
                nanos: 0
            }),
            &Value::Interval(Interval {
                months: 0,
                days: 0,
                nanos: 1
            }),
        ),
        Some(Ordering::Greater)
    );
}

// --- through a real predicate ----------------------------------------------

struct Sch;
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        match name.to_ascii_lowercase().as_str() {
            "id" | "v" => Some(Ty::Int64),
            "d" => Some(Ty::Double),
            _ => None,
        }
    }
    fn is_variable(&self, name: &str) -> bool {
        name == "A"
    }
}

fn ids_matching(define: &str, rows: Vec<Vec<Value>>) -> Vec<i64> {
    let cfg = PlanConfig {
        pattern: "A".into(),
        define_json: define.into(),
        subset_json: String::new(),
        measures_json: Some(r#"{"id":"LAST(id)"}"#.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "to next row".into(),
        step_budget: Some(1_000_000),
    };
    let store = VecRowStore::new(
        vec![("id", Ty::Int64), ("v", Ty::Int64), ("d", Ty::Double)],
        rows,
    );
    Plan::build(&cfg, &Sch)
        .unwrap()
        .run(&store)
        .unwrap()
        .into_iter()
        // Columns: id
        .map(|r| match r[0] {
            Value::Int(i) => i,
            ref other => panic!("expected an id, got {other:?}"),
        })
        .collect()
}

/// The end-to-end shape of the precision bug: an equality predicate that
/// matched a row holding a *different* value.
#[test]
fn a_big_integer_equality_predicate_does_not_match_its_neighbour() {
    let rows = vec![
        vec![
            Value::Int(1),
            Value::Int(9_007_199_254_740_993),
            Value::Double(0.0),
        ],
        vec![
            Value::Int(2),
            Value::Int(9_007_199_254_740_992),
            Value::Double(0.0),
        ],
    ];
    assert_eq!(
        ids_matching(r#"{"A":"v = 9007199254740992"}"#, rows),
        vec![2]
    );
}

/// And the ordering bug: `v > PREV(v)` over adjacent nanosecond-scale values.
#[test]
fn an_ordering_predicate_sees_adjacent_big_integers() {
    let rows = vec![
        vec![
            Value::Int(1),
            Value::Int(1_700_000_000_000_000_000),
            Value::Double(0.0),
        ],
        vec![
            Value::Int(2),
            Value::Int(1_700_000_000_000_000_001),
            Value::Double(0.0),
        ],
    ];
    // Row 1 has no predecessor, so PREV is NULL and the predicate is unknown.
    assert_eq!(ids_matching(r#"{"A":"v > PREV(v)"}"#, rows), vec![2]);
}

/// A NaN in the data must not make a comparison predicate fire.
#[test]
fn a_nan_row_does_not_satisfy_a_comparison_predicate() {
    let rows = vec![
        vec![Value::Int(1), Value::Int(0), Value::Double(f64::NAN)],
        vec![Value::Int(2), Value::Int(0), Value::Double(5.0)],
    ];
    assert_eq!(ids_matching(r#"{"A":"d > 1.0"}"#, rows), vec![2]);
}

// --- the structural guard --------------------------------------------------

/// Pairs of one column's type over the families where SQL equality and the
/// total sort order *coincide*, so the two comparators must agree exactly.
///
/// Floats are not here — see `float_pair` and the weaker property below.
fn exact_pair() -> impl Strategy<Value = (Value, Value)> {
    prop_oneof![
        // BIGINT, plus a band deliberately inside the range where f64 runs out
        // of mantissa: that is where the bug lived, and uniform i64s hit it
        // only rarely.
        (any::<i64>(), any::<i64>()).prop_map(|(a, b)| (Value::Int(a), Value::Int(b))),
        (9_007_199_254_740_000i64..9_007_199_254_741_000, 0i64..1000)
            .prop_map(|(a, d)| (Value::Int(a), Value::Int(a.wrapping_add(d)))),
        (
            1_700_000_000_000_000_000i64..1_700_000_000_000_001_000,
            0i64..1000
        )
            .prop_map(|(a, d)| (Value::Int(a), Value::Int(a.wrapping_add(d)))),
        // HUGEINT, and the mixed pairing with BIGINT.
        (any::<i128>(), any::<i128>()).prop_map(|(a, b)| (Value::HugeInt(a), Value::HugeInt(b))),
        (any::<i64>(), any::<i64>()).prop_map(|(a, b)| (Value::Int(a), Value::HugeInt(b as i128))),
        // VARCHAR, BOOLEAN, DATE, TIMESTAMP.
        (".{0,8}", ".{0,8}").prop_map(|(a, b)| (Value::Str(a), Value::Str(b))),
        (any::<bool>(), any::<bool>()).prop_map(|(a, b)| (Value::Bool(a), Value::Bool(b))),
        (any::<i32>(), any::<i32>()).prop_map(|(a, b)| (Value::Date(a), Value::Date(b))),
        (any::<i64>(), any::<i64>()).prop_map(|(a, b)| (
            Value::Timestamp(a, TimeUnit::Micro),
            Value::Timestamp(b, TimeUnit::Micro)
        )),
    ]
}

/// DOUBLE pairs, including NaN and both zeroes — the values on which the two
/// comparators are *allowed* to differ.
fn float_pair() -> impl Strategy<Value = (Value, Value)> {
    let f = prop_oneof![
        5 => any::<f64>(),
        1 => Just(f64::NAN),
        1 => Just(-f64::NAN),
        1 => Just(0.0f64),
        1 => Just(-0.0f64),
        1 => Just(f64::INFINITY),
        1 => Just(f64::NEG_INFINITY),
    ];
    (f.clone(), f).prop_map(|(a, b)| (Value::Double(a), Value::Double(b)))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    /// **The guard this file exists for.** `compare` (SQL) and `cmp_for_sort`
    /// (the tape order) are separate implementations of one ordering, and they
    /// had silently drifted: sorting compared integers exactly while a DEFINE
    /// predicate compared them as floats, so `ORDER BY` and `x = y` disagreed
    /// about which rows were equal. `mr-worker/tests/sort_agreement.rs` pins
    /// the two *sort* paths to each other; nothing pinned this pair.
    ///
    /// Exact agreement is the right assertion for these types: every one of
    /// them has a single notion of equality.
    #[test]
    fn comparators_agree_exactly((a, b) in exact_pair()) {
        let sql = compare(&a, &b);
        let sorted = cmp_for_sort(&a, &b, false, false);
        prop_assert_eq!(
            sql, Some(sorted),
            "compare({:?}, {:?}) = {:?} but cmp_for_sort said {:?}", a, b, sql, sorted
        );
    }

    /// Floats are the one family where the two are *supposed* to differ, and
    /// both differences are ties rather than contradictions:
    ///
    /// - **NaN** is unordered to SQL (`None`) but has to land somewhere in a
    ///   total order, or a single NaN key would scramble the partition.
    /// - **`-0.0` vs `0.0`** are equal to SQL (IEEE says so) but distinct to
    ///   `total_cmp`, which needs a tiebreak to stay antisymmetric.
    ///
    /// So the invariant is weaker but still worth pinning: they never assert
    /// *opposite* strict orderings. This property found the ±0.0 case, which
    /// is how it came to be documented here.
    #[test]
    fn comparators_never_contradict_on_floats((a, b) in float_pair()) {
        let sql = compare(&a, &b);
        let sorted = cmp_for_sort(&a, &b, false, false);
        if let Some(sql) = sql {
            if sql != Ordering::Equal {
                prop_assert_eq!(
                    sql, sorted,
                    "compare({:?}, {:?}) = {:?} but cmp_for_sort said {:?}", a, b, sql, sorted
                );
            }
        }
    }

    /// Sorting is a total order, so DESC is exactly the reverse of ASC for
    /// non-NULL values — worth pinning next to the above, since the shared
    /// `cmp_ints` helper now sits on both paths.
    #[test]
    fn sort_desc_reverses_asc((a, b) in exact_pair()) {
        prop_assert_eq!(
            cmp_for_sort(&a, &b, true, false),
            cmp_for_sort(&a, &b, false, false).reverse()
        );
    }
}

/// The ties-differ cases above, spelled out as examples so the intent survives
/// a future reading of the weaker property.
#[test]
fn sql_and_sort_differ_only_on_float_ties() {
    // SQL: equal (IEEE). Sort: distinct, so `total_cmp` stays antisymmetric.
    assert_eq!(
        compare(&Value::Double(-0.0), &Value::Double(0.0)),
        Some(Ordering::Equal)
    );
    assert_eq!(
        cmp_for_sort(&Value::Double(-0.0), &Value::Double(0.0), false, false),
        Ordering::Less
    );
    // SQL: unordered. Sort: somewhere definite, or one NaN scrambles the tape.
    assert_eq!(compare(&Value::Double(f64::NAN), &Value::Double(1.0)), None);
    assert_eq!(
        cmp_for_sort(&Value::Double(f64::NAN), &Value::Double(1.0), false, false),
        Ordering::Greater
    );
}

/// `compare` is used as an equality test by `NULLIF` and as an ordering by
/// `GREATEST`/`LEAST`, so exactness has to survive those wrappers too.
#[test]
fn scalar_functions_inherit_exact_comparison() {
    let big = Value::Int(9_007_199_254_740_993);
    let near = Value::Int(9_007_199_254_740_992);
    assert_eq!(
        mr_core::engine::scalar::call("greatest", &[big.clone(), near.clone()]).unwrap(),
        big
    );
    assert_eq!(
        mr_core::engine::scalar::call("least", &[big.clone(), near.clone()]).unwrap(),
        near
    );
    // NULLIF returns NULL only on a genuine equality.
    assert_eq!(
        mr_core::engine::scalar::call("nullif", &[big.clone(), near]).unwrap(),
        big
    );
}

// --- decimal scale corners -------------------------------------------------

/// Arrow permits a negative DECIMAL scale (a `DECIMAL(p, -2)` counts hundreds),
/// which `10i128.pow(scale as u32)` turned into a ~4e9 exponent and a panic.
/// `format_decimal` had always read `scale <= 0` as "already integral", so the
/// divisor is 1 and the value passes through.
#[test]
fn a_negative_decimal_scale_does_not_panic() {
    use mr_core::engine::valops::coerce;
    // CAST(DECIMAL(p,-2) AS BIGINT): the unscaled value is the answer.
    assert_eq!(
        coerce(Value::Decimal(1234, -2), &Ty::Int64).unwrap(),
        Value::Int(1234)
    );
    // ... and through the rounding functions, which shared the same expression.
    for f in ["ceil", "floor", "round"] {
        assert_eq!(
            mr_core::engine::scalar::call(f, &[Value::Decimal(1234, -2)]).unwrap(),
            Value::Decimal(1234, -2),
            "{f} of a negative-scale decimal"
        );
    }
    // Scale 0 is the ordinary integral case and behaves the same way.
    assert_eq!(
        coerce(Value::Decimal(7, 0), &Ty::Int64).unwrap(),
        Value::Int(7)
    );
}

/// The ordinary positive-scale path is unchanged.
#[test]
fn a_positive_decimal_scale_still_divides() {
    use mr_core::engine::valops::coerce;
    // 123.45 -> 123
    assert_eq!(
        coerce(Value::Decimal(12_345, 2), &Ty::Int64).unwrap(),
        Value::Int(123)
    );
    assert_eq!(
        mr_core::engine::scalar::call("ceil", &[Value::Decimal(12_345, 2)]).unwrap(),
        Value::Decimal(12_400, 2)
    );
}
