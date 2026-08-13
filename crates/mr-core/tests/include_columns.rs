//! `include`: input columns carried through to the output.
//!
//! SQL:2016 ALL ROWS PER MATCH passes the input row through; this function emits
//! only the partition keys, the order keys, the automatic columns and the measures,
//! because it buffers only the columns the query reads and an unread column is the
//! most expensive thing it can carry. `include` is the opt-in: naming a column both
//! buffers it and emits it.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch(HashMap<String, Ty>);

impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        name == "A"
    }
}

fn schema() -> Sch {
    Sch([
        ("pid".to_string(), Ty::Int64),
        ("ts".to_string(), Ty::Int64),
        ("v".to_string(), Ty::Int64),
        ("memo".to_string(), Ty::Varchar),
    ]
    .into_iter()
    .collect())
}

/// `(pid, ts, v, memo)`.
fn store() -> VecRowStore {
    VecRowStore::new(
        vec![
            ("pid", Ty::Int64),
            ("ts", Ty::Int64),
            ("v", Ty::Int64),
            ("memo", Ty::Varchar),
        ],
        (0..4i64)
            .map(|i| {
                vec![
                    Value::Int(1),
                    Value::Int(i),
                    Value::Int(i * 10),
                    Value::Str(format!("note{i}")),
                ]
            })
            .collect(),
    )
}

fn cfg(include: &[&str], rows_all: bool) -> PlanConfig {
    PlanConfig {
        pattern: "A+".into(),
        define_json: "{\"A\":\"v >= 0\"}".into(),
        subset_json: String::new(),
        measures_json: Some("{\"n\":\"COUNT(*)\"}".into()),
        partition_by: vec!["pid".into()],
        include: include.iter().map(|s| (*s).to_string()).collect(),
        order_by: vec!["ts".into()],
        rows_all,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    }
}

fn names(plan: &Plan) -> Vec<String> {
    plan.output_columns()
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

#[test]
fn an_included_column_is_emitted_after_the_partition_keys() {
    let plan = Plan::build(&cfg(&["memo"], false), &schema()).unwrap();
    assert_eq!(names(&plan), vec!["pid", "memo", "n"]);
    let out = plan.run(&store()).unwrap();
    // ONE ROW PER MATCH positions passthrough columns on the match's first row, the
    // same row the partition keys are read from.
    assert_eq!(
        out,
        vec![vec![
            Value::Int(1),
            Value::Str("note0".into()),
            Value::Int(4)
        ]]
    );
}

#[test]
fn all_rows_carries_the_column_of_each_matched_row() {
    let plan = Plan::build(&cfg(&["memo"], true), &schema()).unwrap();
    assert_eq!(
        names(&plan),
        vec!["pid", "memo", "ts", "match_number", "classifier", "n"]
    );
    let out = plan.run(&store()).unwrap();
    let memos: Vec<&Value> = out.iter().map(|r| &r[1]).collect();
    assert_eq!(
        memos,
        vec![
            &Value::Str("note0".into()),
            &Value::Str("note1".into()),
            &Value::Str("note2".into()),
            &Value::Str("note3".into()),
        ]
    );
}

/// It must reach `referenced_columns`, or the worker would not buffer it and the
/// column would be missing at produce time rather than at bind time.
#[test]
fn an_included_column_is_a_referenced_column() {
    let plan = Plan::build(&cfg(&["memo"], false), &schema()).unwrap();
    assert!(plan
        .referenced_columns()
        .iter()
        .any(|c| c.eq_ignore_ascii_case("memo")));
}

/// Naming a column that is already emitted is redundant rather than wrong, so it is
/// dropped — two output columns of one name would be worse than either.
#[test]
fn columns_already_emitted_are_not_repeated() {
    let plan = Plan::build(&cfg(&["pid", "memo", "MEMO"], false), &schema()).unwrap();
    assert_eq!(names(&plan), vec!["pid", "memo", "n"]);

    // Under ALL ROWS the order keys are emitted too, so naming one is also a no-op.
    let plan = Plan::build(&cfg(&["ts", "memo"], true), &schema()).unwrap();
    assert_eq!(
        names(&plan),
        vec!["pid", "memo", "ts", "match_number", "classifier", "n"]
    );
}

/// Under ONE ROW the order keys are *not* emitted, so including one is meaningful.
#[test]
fn an_order_key_can_be_included_under_one_row_per_match() {
    let plan = Plan::build(&cfg(&["ts"], false), &schema()).unwrap();
    assert_eq!(names(&plan), vec!["pid", "ts", "n"]);
    let out = plan.run(&store()).unwrap();
    assert_eq!(out[0][1], Value::Int(0), "the match's first row's ts");
}

#[test]
fn an_unknown_included_column_fails_at_bind() {
    let msg = match Plan::build(&cfg(&["nope"], false), &schema()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("an unknown include column must not bind"),
    };
    assert!(msg.contains("nope"), "{msg}");
    assert!(msg.contains("include"), "{msg}");
}
