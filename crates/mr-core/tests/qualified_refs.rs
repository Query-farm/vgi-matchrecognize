//! Regression tests for bare variable-qualified references (Blocker B).
//!
//! `A.col` used outside a navigation/aggregate call is the standard's shorthand
//! for `LAST(A.col)` under the prevailing RUNNING/FINAL semantics. It used to
//! ignore the qualifier and read the *current* row, which silently reduced
//! match-dependent predicates like `"B": "price > A.price"` to `price > price` —
//! wrong answers with no error. These assert the corrected semantics and pin the
//! qualified-navigation and aggregate paths that must not regress.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch {
    cols: HashMap<String, Ty>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| *t)
    }
    fn is_variable(&self, name: &str) -> bool {
        matches!(name.to_ascii_uppercase().as_str(), "A" | "B" | "C")
    }
}

fn sch() -> Sch {
    Sch {
        cols: [
            ("ts".to_string(), Ty::Int64),
            ("amt".to_string(), Ty::Int64),
        ]
        .into_iter()
        .collect(),
    }
}

/// Rows `(ts, amt)` with ts = 0.. and the given amounts.
fn store(amts: &[i64]) -> VecRowStore {
    VecRowStore::new(
        vec![("ts", Ty::Int64), ("amt", Ty::Int64)],
        amts.iter()
            .enumerate()
            .map(|(i, a)| vec![Value::Int(i as i64), Value::Int(*a)])
            .collect(),
    )
}

fn run(pattern: &str, define: &str, measures: &str, amts: &[i64]) -> Vec<Vec<Value>> {
    run_cfg(pattern, define, measures, amts, false)
}

fn run_cfg(
    pattern: &str,
    define: &str,
    measures: &str,
    amts: &[i64],
    rows_all: bool,
) -> Vec<Vec<Value>> {
    let cfg = PlanConfig {
        pattern: pattern.into(),
        define_json: define.into(),
        measures_json: Some(measures.into()),
        partition_by: vec![],
        order_by: vec!["ts".into()],
        rows_all,
        after: "past last row".into(),
        step_budget: Some(1_000_000),
    };
    Plan::build(&cfg, &sch())
        .unwrap()
        .run(&store(amts))
        .unwrap()
}

/// The headline case: a match-dependent predicate comparing against another
/// variable's row. Used to return zero matches because it collapsed to
/// `amt > amt`.
#[test]
fn define_predicate_against_another_variable() {
    // A binds 10; both later rows exceed it, so the greedy B+ takes them all.
    let out = run(
        "A B+",
        r#"{"B":"amt > A.amt"}"#,
        r#"{"n":"COUNT(*)","a":"A.amt","lb":"LAST(B.amt)"}"#,
        &[10, 20, 30],
    );
    assert_eq!(out.len(), 1, "one match spanning all three rows");
    assert_eq!(out[0][0], Value::Int(3), "COUNT(*)");
    assert_eq!(
        out[0][1],
        Value::Int(10),
        "A.amt is the A row, not the last"
    );
    assert_eq!(out[0][2], Value::Int(30), "LAST(B.amt)");
}

/// The predicate must actually *discriminate* — rows failing it are excluded.
#[test]
fn define_predicate_against_another_variable_rejects() {
    // A binds 20. Only 30 exceeds it; the trailing 5 must not join the match.
    let out = run(
        "A B+",
        r#"{"B":"amt > A.amt"}"#,
        r#"{"n":"COUNT(*)","lb":"LAST(B.amt)"}"#,
        &[20, 30, 5],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(2), "A plus a single qualifying B");
    assert_eq!(out[0][1], Value::Int(30));
}

/// A bare qualified ref in MEASURES resolves per variable, not to the current
/// row. Previously every column here returned the last row's value.
#[test]
fn measures_resolve_each_qualifier_separately() {
    let out = run(
        "A B",
        r#"{}"#,
        r#"{"a":"A.amt","b":"B.amt","diff":"B.amt - A.amt"}"#,
        &[10, 20],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(10), "A.amt");
    assert_eq!(out[0][1], Value::Int(20), "B.amt");
    assert_eq!(out[0][2], Value::Int(10), "B.amt - A.amt");
}

/// `A.col` means the *last* row bound to A when A matched several rows (logical
/// LAST, per the standard) — not the first, and not the current row.
#[test]
fn bare_qualified_ref_is_last_not_first() {
    let out = run(
        "A+ B",
        r#"{"A":"amt < 100","B":"amt >= 100"}"#,
        r#"{"la":"A.amt","fa":"FIRST(A.amt)","b":"B.amt"}"#,
        &[1, 2, 3, 100],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(3), "A.amt == LAST(A.amt)");
    assert_eq!(out[0][1], Value::Int(1), "FIRST(A.amt) still the first");
    assert_eq!(out[0][2], Value::Int(100), "B.amt");
}

/// Self-reference: `B.col` inside `DEFINE[B]` is the candidate row itself, so it
/// behaves like the unqualified column.
#[test]
fn self_qualified_ref_in_define_is_the_candidate_row() {
    let qualified = run(
        "A B+",
        r#"{"B":"B.amt > 15"}"#,
        r#"{"n":"COUNT(*)"}"#,
        &[10, 20, 30],
    );
    let unqualified = run(
        "A B+",
        r#"{"B":"amt > 15"}"#,
        r#"{"n":"COUNT(*)"}"#,
        &[10, 20, 30],
    );
    assert_eq!(qualified, unqualified, "B.amt == amt inside DEFINE[B]");
    assert_eq!(qualified[0][0], Value::Int(3));
}

/// A qualifier with no bound row yields NULL rather than a wrong value or an
/// error: here the alternation takes A, leaving C unbound.
#[test]
fn unbound_qualifier_is_null() {
    let out = run(
        "(A | C) B",
        r#"{"A":"amt = 10","C":"amt = 999","B":"amt = 20"}"#,
        r#"{"a":"A.amt","c":"C.amt"}"#,
        &[10, 20],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(10), "A.amt bound");
    assert_eq!(out[0][1], Value::Null, "C.amt unbound -> NULL");
}

/// Under ALL ROWS PER MATCH the default is RUNNING, so a bare qualified ref
/// tracks the last row bound *so far* — NULL before the variable first binds.
#[test]
fn running_semantics_under_all_rows() {
    let out = run_cfg(
        "A B*",
        r#"{"A":"amt = 10"}"#,
        r#"{"ax":"A.amt","bx":"B.amt","fin_b":"FINAL LAST(B.amt)"}"#,
        &[10, 20, 30],
        true,
    );
    assert_eq!(out.len(), 3, "one row per matched row");
    // Columns: ts (order_by), match_number, classifier, ax, bx, fin_b.
    assert_eq!(out[0][3], Value::Int(10), "A.amt at the A row");
    assert_eq!(out[0][4], Value::Null, "B.amt before any B is bound");
    assert_eq!(out[1][4], Value::Int(20), "RUNNING B.amt at the first B");
    assert_eq!(out[2][4], Value::Int(30), "RUNNING B.amt at the second B");
    // FINAL sees the whole match from every row.
    for r in &out {
        assert_eq!(r[5], Value::Int(30), "FINAL LAST(B.amt)");
    }
}

/// Qualified refs inside aggregates keep scoping to that variable's rows.
#[test]
fn aggregates_over_qualified_refs_unchanged() {
    let out = run(
        "A+ B+",
        r#"{"A":"amt < 100","B":"amt >= 100"}"#,
        r#"{"sa":"SUM(A.amt)","sb":"SUM(B.amt)","ca":"COUNT(A.*)","mx":"MAX(A.amt)"}"#,
        &[1, 2, 3, 100, 200],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::HugeInt(6), "SUM(A.amt) = 1+2+3");
    assert_eq!(out[0][1], Value::HugeInt(300), "SUM(B.amt) = 100+200");
    assert_eq!(out[0][2], Value::Int(3), "COUNT(A.*)");
    assert_eq!(out[0][3], Value::Int(3), "MAX(A.amt)");
}

/// Qualified physical navigation anchors on the last row bound to the variable
/// (the standard's `PREV(LAST(A.x), n)`), while unqualified PREV/NEXT still step
/// from the current row.
#[test]
fn qualified_prev_next_anchor_on_the_variable() {
    // Rows: amt 5, 10, 20. The match is A=row1 (amt 10), B=row2 (amt 20), so
    // PREV(A.amt) steps back from row1 to row0 (amt 5).
    let out = run(
        "A B",
        r#"{"A":"amt = 10","B":"amt = 20"}"#,
        r#"{"pa":"PREV(A.amt)","na":"NEXT(A.amt)","p":"PREV(amt)"}"#,
        &[5, 10, 20],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][0],
        Value::Int(5),
        "PREV(A.amt) = row before the A row"
    );
    assert_eq!(
        out[0][1],
        Value::Int(20),
        "NEXT(A.amt) = row after the A row"
    );
    // Unqualified PREV is relative to the current (last matched) row, row2.
    assert_eq!(out[0][2], Value::Int(10), "PREV(amt) from the current row");
}

/// Navigating off the ends of the partition yields NULL, not an error.
#[test]
fn qualified_navigation_off_the_edge_is_null() {
    let out = run(
        "A B",
        r#"{"A":"amt = 10","B":"amt = 20"}"#,
        r#"{"pa":"PREV(A.amt)","nb":"NEXT(B.amt)"}"#,
        &[10, 20],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Null, "no row before the A row");
    assert_eq!(out[0][1], Value::Null, "no row after the B row");
}
