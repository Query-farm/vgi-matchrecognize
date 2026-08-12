//! Prefix `NOT` in a DEFINE predicate, end to end.
//!
//! `tests/expr.rs` pins the AST shape; this pins what the matcher actually does
//! with it. Both are needed: the parser bug here was invisible in the AST unless
//! you knew which shape to expect, but it inverted the row a predicate selected.

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

fn sch() -> Sch {
    Sch {
        cols: [
            ("id".to_string(), Ty::Int64),
            ("v".to_string(), Ty::Int64),
            ("flag".to_string(), Ty::Boolean),
        ]
        .into_iter()
        .collect(),
        labels: vec!["A".to_string()],
    }
}

/// Rows of `(id BIGINT, v BIGINT, flag BOOLEAN)`; `flag: None` is SQL NULL.
fn store(rows: &[(i64, i64, Option<bool>)]) -> VecRowStore {
    VecRowStore::new(
        vec![("id", Ty::Int64), ("v", Ty::Int64), ("flag", Ty::Boolean)],
        rows.iter()
            .map(|(id, v, f)| {
                vec![
                    Value::Int(*id),
                    Value::Int(*v),
                    f.map(Value::Bool).unwrap_or(Value::Null),
                ]
            })
            .collect(),
    )
}

/// Every row that binds `A`, as its `id`. `pattern := "A"` with
/// `AFTER MATCH SKIP TO NEXT ROW` reports one match per matching row.
fn ids_matching(define: &str, rows: &[(i64, i64, Option<bool>)]) -> Vec<i64> {
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
    let plan = Plan::build(&cfg, &sch()).unwrap();
    plan.run(&store(rows))
        .unwrap()
        .into_iter()
        // Columns: id
        .map(|r| match r[0] {
            Value::Int(i) => i,
            ref other => panic!("expected an id, got {other:?}"),
        })
        .collect()
}

/// `NOT flag IS NULL` means `NOT (flag IS NULL)` — it selects the rows whose
/// `flag` is *present*. Parsing `NOT` at the unary level made it
/// `(NOT flag) IS NULL`, which is true exactly when `flag` IS NULL, because
/// `NOT NULL` is NULL. That returned id=2 here instead of id=1.
#[test]
fn not_is_null_predicate_matches_the_non_null_row() {
    let rows = [(1, 10, Some(true)), (2, 20, None)];
    assert_eq!(ids_matching(r#"{"A":"NOT flag IS NULL"}"#, &rows), vec![1]);
}

/// The complement, to show the pair is not accidentally symmetric.
#[test]
fn is_null_predicate_matches_the_null_row() {
    let rows = [(1, 10, Some(true)), (2, 20, None)];
    assert_eq!(ids_matching(r#"{"A":"flag IS NULL"}"#, &rows), vec![2]);
}

/// `NOT v > 10` is `NOT (v > 10)`. Under the old precedence this was
/// `(NOT v) > 10`, which since DEFINE became type-checked at bind is a hard
/// bind error — "NOT requires a BOOLEAN operand, got BIGINT" — so this test
/// would not merely return the wrong rows, it would fail to build at all.
#[test]
fn not_comparison_predicate_binds_and_selects_the_complement() {
    let rows = [(1, 5, Some(true)), (2, 20, Some(true))];
    assert_eq!(ids_matching(r#"{"A":"NOT v > 10"}"#, &rows), vec![1]);
}

/// `NOT` stops at `AND`, so this is `(NOT flag) AND v > 0` — never
/// `NOT (flag AND v > 0)`.
#[test]
fn not_binds_tighter_than_and_in_a_predicate() {
    let rows = [
        (1, 5, Some(false)), // NOT false = true, v > 0 -> binds
        (2, 5, Some(true)),  // NOT true = false -> no
        (3, 0, Some(false)), // v > 0 false -> no
    ];
    assert_eq!(
        ids_matching(r#"{"A":"NOT flag AND v > 0"}"#, &rows),
        vec![1]
    );
}
