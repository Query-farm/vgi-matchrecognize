//! Matcher/evaluator golden scenarios over an in-memory store: sessionization,
//! funnel, alternation, reluctant quantifiers, every AFTER MATCH SKIP mode,
//! ONE vs ALL rows, ties/nulls, and the step-budget abort.

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;
use mr_core::MrError;

struct Sch {
    cols: Vec<(String, Ty)>,
    vars: Vec<String>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, t)| *t)
    }
    fn is_variable(&self, name: &str) -> bool {
        self.vars.iter().any(|v| v.eq_ignore_ascii_case(name))
    }
}

struct Case {
    cols: Vec<(&'static str, Ty)>,
    rows: Vec<Vec<Value>>,
}

impl Case {
    fn run(&self, cfg: PlanConfig, vars: &[&str]) -> mr_core::Result<Vec<Vec<Value>>> {
        let sch = Sch {
            cols: self.cols.iter().map(|(n, t)| (n.to_string(), *t)).collect(),
            vars: vars.iter().map(|s| s.to_ascii_uppercase()).collect(),
        };
        let store = VecRowStore::new(self.cols.clone(), self.rows.clone());
        let plan = Plan::build(&cfg, &sch)?;
        plan.run(&store)
    }
}

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn cfg(
    pattern: &str,
    define: &str,
    measures: Option<&str>,
    order: &str,
    rows_all: bool,
) -> PlanConfig {
    PlanConfig {
        pattern: pattern.into(),
        define_json: define.into(),
        measures_json: measures.map(|m| m.into()),
        partition_by: vec![],
        order_by: vec![order.into()],
        rows_all,
        after: "past last row".into(),
        step_budget: 1_000_000,
    }
}

#[test]
fn sessionization() {
    let case = Case {
        cols: vec![("ts", Ty::Int64)],
        rows: vec![
            vec![i(1)],
            vec![i(2)],
            vec![i(3)],
            vec![i(5000)],
            vec![i(5001)],
        ],
    };
    let out = case
        .run(
            cfg(
                "A B*",
                r#"{"B":"ts <= PREV(ts) + 1800"}"#,
                Some(r#"{"session":"MATCH_NUMBER()","start":"FIRST(ts)","end":"LAST(ts)","n":"COUNT(*)"}"#),
                "ts",
                false,
            ),
            &["A", "B"],
        )
        .unwrap();
    // Two sessions: [1,2,3] and [5000,5001].
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], vec![i(1), i(1), i(3), i(3)]);
    assert_eq!(out[1], vec![i(2), i(5000), i(5001), i(2)]);
}

#[test]
fn funnel_optional_step() {
    // VIEW CART? CHECKOUT PURCHASE — cart optional.
    let define = r#"{"VIEW":"e='view'","CART":"e='cart'","CHECKOUT":"e='checkout'","PURCHASE":"e='purchase'"}"#;
    // Without cart.
    let no_cart = Case {
        cols: vec![("ts", Ty::Int64), ("e", Ty::Varchar)],
        rows: vec![
            vec![i(1), s("view")],
            vec![i(2), s("checkout")],
            vec![i(3), s("purchase")],
        ],
    };
    let out = no_cart
        .run(
            cfg(
                "VIEW CART? CHECKOUT PURCHASE",
                define,
                Some(r#"{"n":"COUNT(*)"}"#),
                "ts",
                false,
            ),
            &["VIEW", "CART", "CHECKOUT", "PURCHASE"],
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], vec![i(3)]); // 3 rows, cart skipped

    // With cart.
    let with_cart = Case {
        cols: vec![("ts", Ty::Int64), ("e", Ty::Varchar)],
        rows: vec![
            vec![i(1), s("view")],
            vec![i(2), s("cart")],
            vec![i(3), s("checkout")],
            vec![i(4), s("purchase")],
        ],
    };
    let out = with_cart
        .run(
            cfg(
                "VIEW CART? CHECKOUT PURCHASE",
                define,
                Some(r#"{"n":"COUNT(*)"}"#),
                "ts",
                false,
            ),
            &["VIEW", "CART", "CHECKOUT", "PURCHASE"],
        )
        .unwrap();
    assert_eq!(out[0], vec![i(4)]);
}

#[test]
fn alternation_branch_backtracking() {
    // A (B | C)+ D over a,b,c,b,d.
    let define = r#"{"A":"e='a'","B":"e='b'","C":"e='c'","D":"e='d'"}"#;
    let case = Case {
        cols: vec![("ts", Ty::Int64), ("e", Ty::Varchar)],
        rows: vec![
            vec![i(1), s("a")],
            vec![i(2), s("b")],
            vec![i(3), s("c")],
            vec![i(4), s("b")],
            vec![i(5), s("d")],
        ],
    };
    let out = case
        .run(
            cfg(
                "A (B | C)+ D",
                define,
                Some(r#"{"v":"CLASSIFIER()"}"#),
                "ts",
                true,
            ),
            &["A", "B", "C", "D"],
        )
        .unwrap();
    // ALL ROWS → 5 rows; classifier column is the auto one at index 1 (ts is order col 0).
    let classifiers: Vec<&Value> = out.iter().map(|r| &r[3]).collect(); // r: ts, match_number, classifier(auto), v
    assert_eq!(
        classifiers,
        vec![&s("A"), &s("B"), &s("C"), &s("B"), &s("D")]
    );
}

#[test]
fn reluctant_vs_greedy() {
    // Both A and B always match (undefined). 4 rows.
    let four = || Case {
        cols: vec![("ts", Ty::Int64)],
        rows: vec![vec![i(0)], vec![i(1)], vec![i(2)], vec![i(3)]],
    };
    // Greedy: A+ grabs as many as possible -> one match of all 4 rows.
    let greedy = four()
        .run(cfg("A+ B", "{}", None, "ts", true), &["A", "B"])
        .unwrap();
    let greedy_mn: Vec<&Value> = greedy.iter().map(|r| &r[1]).collect(); // ts, match_number, classifier
    assert_eq!(greedy_mn, vec![&i(1), &i(1), &i(1), &i(1)]);

    // Reluctant: A+? grabs the minimum -> two matches of 2 rows each.
    let reluctant = four()
        .run(cfg("A+? B", "{}", None, "ts", true), &["A", "B"])
        .unwrap();
    let rel_mn: Vec<&Value> = reluctant.iter().map(|r| &r[1]).collect();
    assert_eq!(rel_mn, vec![&i(1), &i(1), &i(2), &i(2)]);
}

#[test]
fn after_match_skip_modes() {
    let four = Case {
        cols: vec![("ts", Ty::Int64)],
        rows: vec![vec![i(0)], vec![i(1)], vec![i(2)], vec![i(3)]],
    };
    let with_after = |after: &str| {
        let mut c = cfg("A B", "{}", None, "ts", true);
        c.after = after.into();
        four.run(c, &["A", "B"]).unwrap()
    };
    // past last row: non-overlapping -> 2 matches (4 rows).
    assert_eq!(with_after("past last row").len(), 4);
    // to next row: overlapping -> [0,1],[1,2],[2,3] -> 3 matches (6 rows).
    assert_eq!(with_after("to next row").len(), 6);
    // to first/last of B advances to B's row -> overlapping like to-next-row.
    assert_eq!(with_after("to first B").len(), 6);
    assert_eq!(with_after("to last B").len(), 6);
}

#[test]
fn one_vs_all_rows_schema() {
    let case = Case {
        cols: vec![("sym", Ty::Varchar), ("ts", Ty::Int64)],
        rows: vec![vec![s("X"), i(1)], vec![s("X"), i(2)]],
    };
    let mk = |rows_all| PlanConfig {
        pattern: "A B".into(),
        define_json: "{}".into(),
        measures_json: Some(r#"{"n":"COUNT(*)"}"#.into()),
        partition_by: vec!["sym".into()],
        order_by: vec!["ts".into()],
        rows_all,
        after: "past last row".into(),
        step_budget: 1_000_000,
    };
    // ONE ROW: [sym, n] one row.
    let one = case.run(mk(false), &["A", "B"]).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].len(), 2);
    assert_eq!(one[0], vec![s("X"), i(2)]);
    // ALL ROWS: [sym, ts, match_number, classifier, n] two rows.
    let all = case.run(mk(true), &["A", "B"]).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].len(), 5);
    assert_eq!(all[0], vec![s("X"), i(1), i(1), s("A"), i(1)]);
    assert_eq!(all[1], vec![s("X"), i(2), i(1), s("B"), i(2)]);
}

#[test]
fn nulls_in_predicate_and_navigation() {
    // price NULL on row 1 -> A:'price > 5' is NULL (unknown) -> no match there.
    let case = Case {
        cols: vec![("ts", Ty::Int64), ("price", Ty::Int64)],
        rows: vec![
            vec![i(1), i(10)],
            vec![i(2), Value::Null],
            vec![i(3), i(20)],
        ],
    };
    let out = case
        .run(
            cfg(
                "A",
                r#"{"A":"price > 5"}"#,
                Some(r#"{"p":"price"}"#),
                "ts",
                false,
            ),
            &["A"],
        )
        .unwrap();
    // Rows with price>5: ts1 and ts3 (ts2 NULL -> excluded).
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], vec![i(10)]);
    assert_eq!(out[1], vec![i(20)]);
}

#[test]
fn ties_stable_sort() {
    // Equal ts keys keep input order (stable sort): tagged by seq.
    let case = Case {
        cols: vec![("ts", Ty::Int64), ("seq", Ty::Int64)],
        rows: vec![vec![i(1), i(100)], vec![i(1), i(200)], vec![i(1), i(300)]],
    };
    let out = case
        .run(
            cfg(
                "A B C",
                "{}",
                Some(r#"{"a":"FIRST(seq)","c":"LAST(seq)"}"#),
                "ts",
                false,
            ),
            &["A", "B", "C"],
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    // FIRST(seq)=100, LAST(seq)=300 — original order preserved under equal keys.
    assert_eq!(out[0], vec![i(100), i(300)]);
}

#[test]
fn step_budget_aborts_cleanly() {
    // Catastrophic backtracking: (A+)+ B with B never matching on a long A run.
    let n = 40;
    let case = Case {
        cols: vec![("ts", Ty::Int64), ("x", Ty::Int64)],
        rows: (0..n).map(|k| vec![i(k), i(1)]).collect(),
    };
    let mut c = cfg("(A+)+ B", r#"{"A":"x = 1","B":"x = 0"}"#, None, "ts", false);
    c.step_budget = 50_000;
    let err = case.run(c, &["A", "B"]).unwrap_err();
    assert!(matches!(err, MrError::StepBudget(_)), "got {err:?}");
}
