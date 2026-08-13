//! The expression-language and pattern features added for SQL:2016 conformance:
//! scalar functions, `SUBSET` union variables, `array_agg` / `arbitrary`, and
//! case-sensitive double-quoted labels.

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

/// Rows of `(id BIGINT, v BIGINT, s VARCHAR)`.
fn store(rows: &[(i64, i64, &str)]) -> VecRowStore {
    VecRowStore::new(
        vec![("id", Ty::Int64), ("v", Ty::Int64), ("s", Ty::Varchar)],
        rows.iter()
            .map(|(i, v, s)| vec![Value::Int(*i), Value::Int(*v), Value::Str((*s).to_string())])
            .collect(),
    )
}

struct Q<'a> {
    pattern: &'a str,
    define: &'a str,
    subset: &'a str,
    measures: &'a str,
    rows_all: bool,
    labels: &'a [&'a str],
}

impl Default for Q<'_> {
    fn default() -> Self {
        Q {
            pattern: "A+",
            define: "{}",
            subset: "",
            measures: "{}",
            rows_all: false,
            labels: &["A"],
        }
    }
}

fn run(q: Q, rows: &[(i64, i64, &str)]) -> Vec<Vec<Value>> {
    try_run(q, rows).unwrap()
}

fn try_run(q: Q, rows: &[(i64, i64, &str)]) -> mr_core::error::Result<Vec<Vec<Value>>> {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: q.pattern.into(),
        define_json: q.define.into(),
        subset_json: q.subset.into(),
        measures_json: Some(q.measures.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: q.rows_all,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(1_000_000),
    };
    let sch = Sch {
        cols: [
            ("id".to_string(), Ty::Int64),
            ("v".to_string(), Ty::Int64),
            ("s".to_string(), Ty::Varchar),
        ]
        .into_iter()
        .collect(),
        labels: q.labels.iter().map(|s| s.to_string()).collect(),
    };
    Plan::build(&cfg, &sch)?.run(&store(rows))
}

// ---------------------------------------------------------------- scalar functions

#[test]
fn scalar_functions_in_measures() {
    let out = run(
        Q {
            measures: r#"{"a":"abs(0 - LAST(v))","lo":"lower(LAST(s))","up":"upper(LAST(s))",
                          "len":"length(LAST(s))","r":"round(LAST(v) / 2)","co":"coalesce(NULL, LAST(v))",
                          "g":"greatest(LAST(v), 5)","l":"least(LAST(v), 5)"}"#,
            define: r#"{"A":"v > 0"}"#,
            ..Default::default()
        },
        &[(1, 3, "Ab")],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(3), "abs");
    assert_eq!(out[0][1], Value::Str("ab".into()), "lower");
    assert_eq!(out[0][2], Value::Str("AB".into()), "upper");
    assert_eq!(out[0][3], Value::Int(2), "length");
    assert_eq!(out[0][4], Value::Double(2.0), "round(1.5)");
    assert_eq!(out[0][5], Value::Int(3), "coalesce skips NULL");
    assert_eq!(out[0][6], Value::Int(5), "greatest");
    assert_eq!(out[0][7], Value::Int(3), "least");
}

/// A scalar function inside a DEFINE predicate actually drives matching.
#[test]
fn scalar_function_in_define_discriminates() {
    let out = run(
        Q {
            pattern: "A+",
            // abs() makes -5 qualify and 1 not.
            define: r#"{"A":"abs(v) >= 5"}"#,
            measures: r#"{"n":"COUNT(*)","first":"FIRST(id)"}"#,
            ..Default::default()
        },
        &[(1, -5, "x"), (2, 7, "x"), (3, 1, "x"), (4, -9, "x")],
    );
    assert_eq!(out.len(), 2, "two runs of qualifying rows");
    assert_eq!(
        (out[0][0].clone(), out[0][1].clone()),
        (Value::Int(2), Value::Int(1))
    );
    assert_eq!(
        (out[1][0].clone(), out[1][1].clone()),
        (Value::Int(1), Value::Int(4))
    );
}

#[test]
fn scalar_functions_propagate_null_and_reject_bad_types() {
    // NULL in, NULL out (coalesce excepted, tested above). `nullif(v, v)` is a
    // typed NULL — a bare `NULL` literal has no type, which is separately a bind
    // error for a measure.
    let out = run(
        Q {
            measures: r#"{"a":"abs(nullif(LAST(v), LAST(v)))","u":"upper(nullif(LAST(s), LAST(s)))"}"#,
            define: r#"{"A":"v > 0"}"#,
            ..Default::default()
        },
        &[(1, 1, "x")],
    );
    assert_eq!(out[0][0], Value::Null, "abs(NULL) is NULL");
    assert_eq!(out[0][1], Value::Null, "upper(NULL) is NULL");

    // An untyped NULL argument still reports the inference error, not a panic.
    let err = try_run(
        Q {
            measures: r#"{"a":"abs(NULL)"}"#,
            define: r#"{"A":"v > 0"}"#,
            ..Default::default()
        },
        &[(1, 1, "x")],
    )
    .expect_err("untyped NULL measure");
    assert!(err.to_string().contains("could not infer a type"));

    // Unknown function and wrong arity are bind errors, not silent NULLs.
    for bad in [r#"{"m":"nosuchfn(v)"}"#, r#"{"m":"abs(v, v)"}"#] {
        let err = try_run(
            Q {
                measures: bad,
                define: r#"{"A":"v > 0"}"#,
                ..Default::default()
            },
            &[(1, 1, "x")],
        )
        .expect_err("should be rejected at bind time");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown function") || msg.contains("takes"),
            "unexpected error: {msg}"
        );
    }
}

// ---------------------------------------------------------------------- SUBSET

#[test]
fn subset_qualifier_ranges_over_members() {
    // U = (A, C): `U.v` is the last row bound to A or C, COUNT(U.*) counts both.
    let out = run(
        Q {
            pattern: "A B C",
            define: r#"{"A":"v = 1","B":"v = 2","C":"v = 3"}"#,
            subset: r#"{"U":["A","C"]}"#,
            measures: r#"{"lu":"U.v","nu":"COUNT(U.*)","su":"SUM(U.v)","fu":"FIRST(U.v)","cu":"CLASSIFIER(U)"}"#,
            labels: &["A", "B", "C", "U"],
            ..Default::default()
        },
        &[(1, 1, "a"), (2, 2, "b"), (3, 3, "c")],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(3), "U.v == LAST over A|C");
    assert_eq!(out[0][1], Value::Int(2), "COUNT(U.*) counts A and C");
    assert_eq!(out[0][2], Value::HugeInt(4), "SUM(U.v) = 1 + 3");
    assert_eq!(out[0][3], Value::Int(1), "FIRST(U.v)");
    assert_eq!(
        out[0][4],
        Value::Str("C".into()),
        "CLASSIFIER(U) is the last U row"
    );
}

/// Inside an aggregate over a subset, a qualified ref must read the *pinned* row,
/// not re-resolve to the subset's last row.
#[test]
fn subset_aggregate_reads_each_member_row() {
    let out = run(
        Q {
            pattern: "A B C",
            define: r#"{"A":"v = 1","B":"v = 2","C":"v = 3"}"#,
            subset: r#"{"U":["A","C"]}"#,
            measures: r#"{"vals":"array_agg(U.v)","tags":"array_agg(CLASSIFIER(U))"}"#,
            labels: &["A", "B", "C", "U"],
            ..Default::default()
        },
        &[(1, 1, "a"), (2, 2, "b"), (3, 3, "c")],
    );
    assert_eq!(
        out[0][0],
        Value::List(vec![Value::Int(1), Value::Int(3)]),
        "each element reads its own row"
    );
    assert_eq!(
        out[0][1],
        Value::List(vec![Value::Str("A".into()), Value::Str("C".into())])
    );
}

#[test]
fn subset_validation() {
    let base = Q {
        pattern: "A B",
        define: r#"{"A":"v = 1","B":"v = 2"}"#,
        measures: r#"{"n":"COUNT(*)"}"#,
        labels: &["A", "B", "U"],
        ..Default::default()
    };
    // A member that is not in the pattern.
    let err = try_run(
        Q {
            subset: r#"{"U":["A","Z"]}"#,
            ..base
        },
        &[(1, 1, "a")],
    )
    .expect_err("unknown member");
    assert!(err.to_string().contains("does not appear in the pattern"));

    // A subset name colliding with a pattern variable.
    let err = try_run(
        Q {
            subset: r#"{"A":["B"]}"#,
            ..base
        },
        &[(1, 1, "a")],
    )
    .expect_err("collision");
    assert!(err.to_string().contains("collides"));
}

// ------------------------------------------------------------------- array_agg

#[test]
fn array_agg_collects_in_match_order() {
    let out = run(
        Q {
            define: r#"{"A":"v > 0"}"#,
            measures: r#"{"vals":"array_agg(v)","tags":"array_agg(CLASSIFIER())","arb":"arbitrary(v)"}"#,
            ..Default::default()
        },
        &[(1, 10, "x"), (2, 20, "x"), (3, 30, "x")],
    );
    assert_eq!(
        out[0][0],
        Value::List(vec![Value::Int(10), Value::Int(20), Value::Int(30)])
    );
    assert_eq!(
        out[0][1].clone(),
        Value::List(vec![
            Value::Str("A".into()),
            Value::Str("A".into()),
            Value::Str("A".into()),
        ])
    );
    assert_eq!(
        out[0][2],
        Value::Int(10),
        "arbitrary takes the first non-NULL"
    );
}

/// Under ALL ROWS PER MATCH the default is RUNNING, so the list grows row by row.
#[test]
fn array_agg_running_grows() {
    let out = run(
        Q {
            define: r#"{"A":"v > 0"}"#,
            measures: r#"{"vals":"array_agg(v)"}"#,
            rows_all: true,
            ..Default::default()
        },
        &[(1, 10, "x"), (2, 20, "x")],
    );
    assert_eq!(out.len(), 2);
    // Columns: id (order_by), match_number, classifier, vals.
    assert_eq!(out[0][3], Value::List(vec![Value::Int(10)]));
    assert_eq!(out[1][3], Value::List(vec![Value::Int(10), Value::Int(20)]));
}

/// An empty match aggregates to an empty list, which is distinct from NULL.
#[test]
fn array_agg_of_empty_match_is_an_empty_list() {
    let out = run(
        Q {
            pattern: "B*",
            define: r#"{"B":"v < PREV(v)"}"#,
            measures: r#"{"n":"COUNT(*)","vals":"array_agg(v)"}"#,
            labels: &["B"],
            ..Default::default()
        },
        &[(1, 90, "x"), (2, 80, "x")],
    );
    // Row 1 cannot bind B (no PREV), so `B*` matches empty there.
    assert_eq!(out[0][0], Value::Int(0), "empty match");
    assert_eq!(out[0][1], Value::List(vec![]), "empty list, not NULL");
    assert_eq!(out[1][1], Value::List(vec![Value::Int(80)]));
}

#[test]
fn array_agg_infers_a_list_type() {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: "A+".into(),
        define_json: r#"{"A":"v > 0"}"#.into(),
        subset_json: String::new(),
        measures_json: Some(r#"{"vals":"array_agg(v)","tags":"array_agg(CLASSIFIER())"}"#.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    let sch = Sch {
        cols: [
            ("id".to_string(), Ty::Int64),
            ("v".to_string(), Ty::Int64),
            ("s".to_string(), Ty::Varchar),
        ]
        .into_iter()
        .collect(),
        labels: vec!["A".to_string()],
    };
    let plan = Plan::build(&cfg, &sch).unwrap();
    let cols = plan.output_columns();
    assert_eq!(cols[0].ty, Ty::List(Box::new(Ty::Int64)));
    assert_eq!(cols[1].ty, Ty::List(Box::new(Ty::Varchar)));
}

// -------------------------------------------------------------- quoted labels

/// `"b"` is case-sensitive and distinct from the unquoted `b` (which is `B`).
#[test]
fn quoted_labels_are_case_sensitive() {
    let out = run(
        Q {
            pattern: r#"a "b"+ C+"#,
            define: r#"{"b":"\"b\".v < PREV(\"b\".v)","C":"C.v > PREV(C.v)"}"#,
            measures: r#"{"label":"CLASSIFIER()"}"#,
            rows_all: true,
            labels: &["A", "b", "C"],
            ..Default::default()
        },
        &[(1, 90, "x"), (2, 80, "x"), (3, 70, "x"), (4, 80, "x")],
    );
    let labels: Vec<&Value> = out.iter().map(|r| &r[3]).collect();
    assert_eq!(
        labels,
        vec![
            &Value::Str("A".into()),
            &Value::Str("b".into()),
            &Value::Str("b".into()),
            &Value::Str("C".into()),
        ],
        "unquoted `a` canonicalizes to A; quoted `\"b\"` keeps its case"
    );
}

/// `count()` with no argument means `count(*)`.
#[test]
fn count_with_no_argument() {
    let out = run(
        Q {
            define: r#"{"A":"v > 0"}"#,
            measures: r#"{"a":"count()","b":"count(*)"}"#,
            ..Default::default()
        },
        &[(1, 1, "x"), (2, 2, "x")],
    );
    assert_eq!(out[0][0], Value::Int(2));
    assert_eq!(out[0][0], out[0][1]);
}
