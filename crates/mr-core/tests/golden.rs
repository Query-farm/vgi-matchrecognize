//! End-to-end golden cases over an in-memory `VecRowStore`, asserting exact
//! output rows (measures + CLASSIFIER + MATCH_NUMBER).

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

/// A bind schema backed by a name->Ty map plus the pattern variable set.
struct Sch {
    cols: HashMap<String, Ty>,
    vars: Vec<String>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        self.vars.iter().any(|v| v.eq_ignore_ascii_case(name))
    }
}

fn sch(cols: &[(&str, Ty)], vars: &[&str]) -> Sch {
    Sch {
        cols: cols
            .iter()
            .map(|(n, t)| (n.to_string(), t.clone()))
            .collect(),
        vars: vars.iter().map(|s| s.to_ascii_uppercase()).collect(),
    }
}

#[test]
fn v_shape_one_row() {
    // ticks: symbol, ts, price — a single V then a rise.
    let store = VecRowStore::new(
        vec![
            ("symbol", Ty::Varchar),
            ("ts", Ty::Int64),
            ("price", Ty::Int64),
        ],
        vec![
            row(&["ACME", "1", "10"]),
            row(&["ACME", "2", "8"]),
            row(&["ACME", "3", "6"]),
            row(&["ACME", "4", "9"]),
            row(&["ACME", "5", "11"]),
        ],
    );
    let cfg = PlanConfig {
        pattern: "START DOWN+ UP+".into(),
        define_json: r#"{"DOWN":"price < PREV(price)","UP":"price > PREV(price)"}"#.into(),
        subset_json: String::new(),
        measures_json: Some(
            r#"{"match_no":"MATCH_NUMBER()","start_price":"FIRST(START.price)",
                "bottom":"LAST(DOWN.price)","drawdown":"FIRST(START.price) - LAST(DOWN.price)"}"#
                .into(),
        ),
        partition_by: vec!["symbol".into()],
        order_by: vec!["ts".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(1_000_000),
    };
    let plan = Plan::build(
        &cfg,
        &sch(
            &[
                ("symbol", Ty::Varchar),
                ("ts", Ty::Int64),
                ("price", Ty::Int64),
            ],
            &["START", "DOWN", "UP"],
        ),
    )
    .unwrap();
    let out = plan.run(&store).unwrap();
    assert_eq!(out.len(), 1, "exactly one V match");
    // Columns: symbol, match_no, start_price, bottom, drawdown
    let r = &out[0];
    assert_eq!(r[0], Value::Str("ACME".into()));
    assert_eq!(r[1], Value::Int(1)); // match_no
    assert_eq!(r[2], Value::Int(10)); // FIRST(START.price)
    assert_eq!(r[3], Value::Int(6)); // LAST(DOWN.price)
    assert_eq!(r[4], Value::Int(4)); // drawdown 10-6
}

#[test]
fn login_fail_then_success_all_rows() {
    // outcome: 3 fails then a success → FAIL{3,} OK
    let store = VecRowStore::new(
        vec![
            ("uid", Ty::Varchar),
            ("ts", Ty::Int64),
            ("outcome", Ty::Varchar),
        ],
        vec![
            row(&["u1", "1", "fail"]),
            row(&["u1", "2", "fail"]),
            row(&["u1", "3", "fail"]),
            row(&["u1", "4", "success"]),
        ],
    );
    let cfg = PlanConfig {
        pattern: "FAIL{3,} OK".into(),
        define_json: r#"{"FAIL":"outcome = 'fail'","OK":"outcome = 'success'"}"#.into(),
        subset_json: String::new(),
        measures_json: Some(r#"{"var":"CLASSIFIER()","n_fails":"FINAL COUNT(FAIL.*)"}"#.into()),
        partition_by: vec!["uid".into()],
        order_by: vec!["ts".into()],
        rows_all: true,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(1_000_000),
    };
    let plan = Plan::build(
        &cfg,
        &sch(
            &[
                ("uid", Ty::Varchar),
                ("ts", Ty::Int64),
                ("outcome", Ty::Varchar),
            ],
            &["FAIL", "OK"],
        ),
    )
    .unwrap();
    let out = plan.run(&store).unwrap();
    // ALL ROWS → 4 output rows.
    // Columns: uid, ts, match_number(auto), classifier(auto), var(measure), n_fails
    assert_eq!(out.len(), 4);
    assert_eq!(
        out[0],
        vec![
            Value::Str("u1".into()),
            Value::Int(1),
            Value::Int(1),
            Value::Str("FAIL".into()),
            Value::Str("FAIL".into()),
            Value::Int(3),
        ]
    );
    // classifier (auto col r[3]) and the CLASSIFIER() measure (r[4]) agree.
    assert_eq!(out[3][3], Value::Str("OK".into()));
    assert_eq!(out[3][4], Value::Str("OK".into()));
    // FINAL count of FAIL is 3 on every row.
    for r in &out {
        assert_eq!(r[5], Value::Int(3));
    }
}

fn row(vals: &[&str]) -> Vec<Value> {
    // Heuristic: parse integers, else string.
    vals.iter()
        .map(|s| match s.parse::<i64>() {
            Ok(n) => Value::Int(n),
            Err(_) => Value::Str(s.to_string()),
        })
        .collect()
}
