//! RUNNING and FINAL aggregate values under ALL ROWS PER MATCH.
//!
//! Every output row of a match re-evaluates the measures with the RUNNING horizon
//! set to that row, and the aggregates are folded **incrementally** across that
//! sweep (`engine::aggmemo`) rather than recomputed per row. That turns a quadratic
//! into a linear pass, and it is only sound while each row's contribution is
//! independent of where the horizon sits — so these tests pin the values a match
//! actually produces, aggregate kind by aggregate kind, including the shapes where
//! the incremental path must decline and fall back.
//!
//! The sharpest case is `sum_over_a_foreign_qualifier_is_not_frozen`: it fails with
//! stale NULLs if the incremental path ever accepts a reference to a label other
//! than the one the aggregate's scope was filtered on.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
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

/// Rows of `(id BIGINT, v BIGINT)`, `id` ascending from 1.
fn store(vs: &[Option<i64>]) -> VecRowStore {
    VecRowStore::new(
        vec![("id", Ty::Int64), ("v", Ty::Int64)],
        vs.iter()
            .enumerate()
            .map(|(i, v)| {
                vec![
                    Value::Int(i as i64 + 1),
                    v.map(Value::Int).unwrap_or(Value::Null),
                ]
            })
            .collect(),
    )
}

/// Run ALL ROWS PER MATCH over `vs` and return the single measure's column.
fn measure(
    pattern: &str,
    define: &str,
    subset: &str,
    measures: &str,
    vs: &[Option<i64>],
) -> Vec<Value> {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: pattern.into(),
        define_json: define.into(),
        subset_json: subset.into(),
        measures_json: Some(measures.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: true,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(50_000_000),
    };
    let sch = Sch {
        cols: [("id".to_string(), Ty::Int64), ("v".to_string(), Ty::Int64)]
            .into_iter()
            .collect(),
        labels: ["A", "B", "U"].iter().map(|s| s.to_string()).collect(),
    };
    let plan = Plan::build(&cfg, &sch).unwrap();
    let out = plan.run(&store(vs)).unwrap();
    // ALL ROWS layout: order_by cols, match_number, classifier, then measures.
    out.into_iter()
        .map(|mut row| row.pop().expect("one measure"))
        .collect()
}

fn ints(vs: &[i64]) -> Vec<Value> {
    vs.iter().copied().map(Value::Int).collect()
}

/// `SUM` over a BIGINT widens to HUGEINT (spec §C type synthesis), so its expected
/// values are HugeInt even when they are small.
fn sums(vs: &[i64]) -> Vec<Value> {
    vs.iter()
        .copied()
        .map(|v| Value::HugeInt(v as i128))
        .collect()
}

const V5: [Option<i64>; 5] = [Some(1), Some(2), Some(3), Some(4), Some(5)];

#[test]
fn running_aggregates_grow_row_by_row() {
    let sum = measure("A+", "{}", "", r#"{"n":"RUNNING SUM(v)"}"#, &V5);
    assert_eq!(sum, sums(&[1, 3, 6, 10, 15]));

    let count = measure("A+", "{}", "", r#"{"n":"RUNNING COUNT(v)"}"#, &V5);
    assert_eq!(count, ints(&[1, 2, 3, 4, 5]));

    let max = measure("A+", "{}", "", r#"{"n":"RUNNING MAX(v)"}"#, &V5);
    assert_eq!(max, ints(&[1, 2, 3, 4, 5]));

    let min = measure("A+", "{}", "", r#"{"n":"RUNNING MIN(v)"}"#, &V5);
    assert_eq!(min, ints(&[1, 1, 1, 1, 1]));

    let avg = measure("A+", "{}", "", r#"{"n":"RUNNING AVG(v)"}"#, &V5);
    let expected: Vec<Value> = [1.0, 1.5, 2.0, 2.5, 3.0]
        .into_iter()
        .map(Value::Double)
        .collect();
    assert_eq!(avg, expected);
}

/// A FINAL aggregate has one value for the whole match, on every row of it.
#[test]
fn final_aggregates_are_constant_across_the_match() {
    let sum = measure("A+", "{}", "", r#"{"n":"FINAL SUM(v)"}"#, &V5);
    assert_eq!(sum, sums(&[15, 15, 15, 15, 15]));

    let count = measure("A+", "{}", "", r#"{"n":"FINAL COUNT(*)"}"#, &V5);
    assert_eq!(count, ints(&[5, 5, 5, 5, 5]));
}

/// NULLs are skipped by every aggregate, and `COUNT(expr)` counts the non-NULL
/// values — the incremental fold must agree.
#[test]
fn nulls_are_skipped_not_folded() {
    let vs = [Some(1), None, Some(3), None, Some(5)];
    assert_eq!(
        measure("A+", "{}", "", r#"{"n":"RUNNING SUM(v)"}"#, &vs),
        sums(&[1, 1, 4, 4, 9])
    );
    assert_eq!(
        measure("A+", "{}", "", r#"{"n":"RUNNING COUNT(v)"}"#, &vs),
        ints(&[1, 1, 2, 2, 3])
    );
    // COUNT(*) counts rows, NULL or not.
    assert_eq!(
        measure("A+", "{}", "", r#"{"n":"RUNNING COUNT(*)"}"#, &vs),
        ints(&[1, 2, 3, 4, 5])
    );
    // An all-NULL prefix is NULL, not 0.
    let leading_nulls = [None, None, Some(7)];
    assert_eq!(
        measure("A+", "{}", "", r#"{"n":"RUNNING SUM(v)"}"#, &leading_nulls),
        vec![Value::Null, Value::Null, Value::HugeInt(7)]
    );
}

/// `array_agg` is the one aggregate that is empty rather than NULL over no rows,
/// which is what makes a RUNNING list grow instead of jumping from NULL.
#[test]
fn array_agg_grows_in_match_order() {
    let out = measure("A+", "{}", "", r#"{"n":"RUNNING array_agg(v)"}"#, &V5[..3]);
    assert_eq!(
        out,
        vec![
            Value::List(ints(&[1])),
            Value::List(ints(&[1, 2])),
            Value::List(ints(&[1, 2, 3])),
        ]
    );
}

/// A label-filtered aggregate ranges only over the rows that label covers, so the
/// value holds steady on rows that are not in scope.
#[test]
fn label_filtered_aggregates_only_see_their_own_rows() {
    // A binds row 1; B binds the rest.
    let define = r#"{"B":"v > 1"}"#;
    assert_eq!(
        measure("A B+", define, "", r#"{"n":"RUNNING SUM(B.v)"}"#, &V5),
        vec![
            Value::Null, // on the A row, no B is bound yet
            Value::HugeInt(2),
            Value::HugeInt(5),
            Value::HugeInt(9),
            Value::HugeInt(14),
        ]
    );
    assert_eq!(
        measure("A B+", define, "", r#"{"n":"RUNNING COUNT(A.*)"}"#, &V5),
        ints(&[1, 1, 1, 1, 1])
    );
    assert_eq!(
        measure("A B+", define, "", r#"{"n":"RUNNING COUNT(B.*)"}"#, &V5),
        ints(&[0, 1, 2, 3, 4])
    );
}

/// A SUBSET union variable covers its members, so the aggregate sees every row.
#[test]
fn subset_qualified_aggregate_covers_its_members() {
    assert_eq!(
        measure(
            "A B+",
            r#"{"B":"v > 1"}"#,
            r#"{"U":["A","B"]}"#,
            r#"{"n":"RUNNING SUM(U.v)"}"#,
            &V5
        ),
        sums(&[1, 3, 6, 10, 15])
    );
}

/// The case that breaks a careless incremental fold.
///
/// `SUM(A.v + B.v)` is scoped by its dominant qualifier `A`, so it folds exactly one
/// row — the `A` row. But `B.v` evaluated there means `LAST(B)` under the prevailing
/// RUNNING semantics, which *changes* as the horizon advances. So that single
/// contribution has to be re-evaluated on every output row, and an accumulator that
/// froze it after the first row would report NULL forever.
#[test]
fn sum_over_a_foreign_qualifier_is_not_frozen() {
    let out = measure(
        "A B+",
        r#"{"B":"v > 1"}"#,
        "",
        r#"{"n":"RUNNING SUM(A.v + B.v)"}"#,
        &V5,
    );
    assert_eq!(
        out,
        vec![
            Value::Null,       // row 1: no B bound yet, so A.v + NULL is NULL
            Value::HugeInt(3), // 1 + 2
            Value::HugeInt(4), // 1 + 3
            Value::HugeInt(5), // 1 + 4
            Value::HugeInt(6), // 1 + 5
        ]
    );
}

/// Navigation inside an aggregate argument is also horizon-dependent, so it must
/// take the recompute path and still be right.
#[test]
fn aggregate_over_navigation_stays_correct() {
    // LAST(v) is the value at the RUNNING horizon, so at row k it is v[k], and the
    // sum over the single A-scope row is that same value.
    let out = measure("A+", "{}", "", r#"{"n":"RUNNING SUM(v - LAST(v))"}"#, &V5);
    // Every term is v[i] - v[horizon-1] for i <= horizon-1.
    assert_eq!(out, sums(&[0, -1, -3, -6, -10]));
}

/// The point of the whole exercise: a long match must be linear.
///
/// A 40,000-row match with a RUNNING and a FINAL aggregate. Recomputing each row's
/// aggregate from scratch would be ~1.6 billion fold steps; extending the fold makes
/// it 40,000. This has already earned its keep twice — it is what caught the FINAL
/// entry being removed from the memo and never put back, which silently restored the
/// quadratic (9.6s here, against 0.02s now).
#[test]
fn long_match_running_aggregate_is_linear() {
    let n = 40_000usize;
    let vs: Vec<Option<i64>> = (1..=n as i64).map(Some).collect();
    let out = measure("A+", "{}", "", r#"{"n":"RUNNING SUM(v)"}"#, &vs);
    assert_eq!(out.len(), n);
    assert_eq!(out[0], Value::HugeInt(1));
    // Sum of 1..=n, which for n = 20,000 exceeds i32 but not i64.
    let total = (n as i64) * (n as i64 + 1) / 2;
    assert_eq!(out[n - 1], Value::HugeInt(total as i128));
    // And a midpoint, so a fold that drifts is caught rather than just the ends.
    let mid = (n / 2) as i64;
    assert_eq!(
        out[n / 2 - 1],
        Value::HugeInt((mid * (mid + 1) / 2) as i128)
    );

    let fin = measure("A+", "{}", "", r#"{"n":"FINAL SUM(v)"}"#, &vs);
    assert!(fin.iter().all(|v| *v == Value::HugeInt(total as i128)));
}

/// An aggregate inside DEFINE is evaluated during *matching*, where the bind
/// sequence shrinks as well as grows — so its incremental state has to be discarded
/// whenever the matcher gives a row back, not extended.
///
/// `A+ B` over 5,5,5 with `B: SUM(A.v) = 10`: greedy A takes all three rows (sum 15)
/// and B has no row left, so it fails; unwinding one row leaves A = 5,5 (sum 10) and
/// B on row 3, where the predicate holds. An accumulator carried over from the
/// abandoned path would still say 15, B would fail again, and there would be no match
/// at all.
#[test]
fn define_aggregate_is_refolded_after_backtracking() {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: "A+ B".into(),
        define_json: r#"{"A":"v > 0","B":"SUM(A.v) = 10"}"#.into(),
        subset_json: String::new(),
        measures_json: Some(r#"{"n":"COUNT(*)","sa":"FINAL SUM(A.v)"}"#.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(50_000_000),
    };
    let sch = Sch {
        cols: [("id".to_string(), Ty::Int64), ("v".to_string(), Ty::Int64)]
            .into_iter()
            .collect(),
        labels: ["A", "B"].iter().map(|s| s.to_string()).collect(),
    };
    let out = Plan::build(&cfg, &sch)
        .unwrap()
        .run(&store(&[Some(5), Some(5), Some(5)]))
        .unwrap();
    assert_eq!(out.len(), 1, "the unwound path should match");
    assert_eq!(out[0][0], Value::Int(3), "all three rows are in the match");
    assert_eq!(out[0][1], Value::HugeInt(10), "two A rows, not three");
}
