//! Label lookups (`LAST(A.x)`, i.e. a bare `A.x`) under backtracking.
//!
//! Where each label was last bound is now tracked incrementally next to `binds`
//! (`engine::bindindex`) instead of being found by scanning the match backwards. That
//! index has to shrink in exact lockstep with the binds whenever the matcher gives a
//! row back — a stale entry would make `A.x` resolve to a row that is no longer part
//! of the match, which is a wrong answer rather than a slow one.
//!
//! So these drive shapes that *must* backtrack: a greedy quantifier that overshoots
//! and unwinds, an alternation whose first branch dies after binding, and the same
//! through a SUBSET (where one row is indexed under several labels).

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch {
    labels: Vec<String>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        match name.to_ascii_lowercase().as_str() {
            "id" | "v" => Some(Ty::Int64),
            _ => None,
        }
    }
    fn is_variable(&self, name: &str) -> bool {
        self.labels.iter().any(|v| v == name)
    }
}

fn store(vs: &[i64]) -> VecRowStore {
    VecRowStore::new(
        vec![("id", Ty::Int64), ("v", Ty::Int64)],
        vs.iter()
            .enumerate()
            .map(|(i, v)| vec![Value::Int(i as i64 + 1), Value::Int(*v)])
            .collect(),
    )
}

/// ONE ROW PER MATCH; returns each match's measure values.
fn run(pattern: &str, define: &str, subset: &str, measures: &str, vs: &[i64]) -> Vec<Vec<Value>> {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: pattern.into(),
        define_json: define.into(),
        subset_json: subset.into(),
        measures_json: Some(measures.into()),
        partition_by: vec![],
        order_by: vec!["id".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: Some(50_000_000),
    };
    let sch = Sch {
        labels: ["A", "B", "C", "U"].iter().map(|s| s.to_string()).collect(),
    };
    Plan::build(&cfg, &sch).unwrap().run(&store(vs)).unwrap()
}

/// A greedy `A+` overshoots and has to give rows back before `B` can match, so the
/// `A.v` in B's predicate must see the *shortened* run of A rows.
///
/// `A+ B` over 5,4,3,9 with `B: v > A.v`: greedy A takes all four rows, B then has no
/// row left and fails; unwinding one row leaves A = 5,4,3 and B = 9, and 9 > 3 holds.
/// If the label index still held the given-back row, `A.v` would resolve to 9 — the
/// row B itself is standing on — and `9 > 9` would fail the match entirely.
#[test]
fn greedy_unwind_resolves_last_to_the_shortened_run() {
    let out = run(
        "A+ B",
        r#"{"A":"v > 0","B":"v > A.v"}"#,
        "",
        r#"{"n":"COUNT(*)","lastA":"FINAL LAST(A.v)","b":"FINAL LAST(B.v)"}"#,
        &[5, 4, 3, 9],
    );
    assert_eq!(out.len(), 1, "expected exactly one match");
    assert_eq!(out[0][0], Value::Int(4), "all four rows should be matched");
    assert_eq!(
        out[0][1],
        Value::Int(3),
        "LAST(A) is the last A row, not B's"
    );
    assert_eq!(out[0][2], Value::Int(9));
}

/// An alternation whose first branch binds a row and then fails: the second branch
/// must not see the abandoned binding.
///
/// `((A B) | (A C))+` over 1,9: the `A B` branch binds A=1 then needs B, and with
/// `B: v < A.v` row 2 (9 < 1) fails, so the branch is abandoned and `A C` is tried
/// with `C: v > A.v` (9 > 1) — which holds. A stale index entry from the dead branch
/// would leave two A rows recorded where the match has one.
#[test]
fn abandoned_branch_leaves_no_binding_behind() {
    let out = run(
        "(A B) | (A C)",
        r#"{"A":"v > 0","B":"v < A.v","C":"v > A.v"}"#,
        "",
        r#"{"n":"COUNT(*)","a":"FINAL LAST(A.v)","nb":"FINAL COUNT(B.*)","nc":"FINAL COUNT(C.*)"}"#,
        &[1, 9],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(2));
    assert_eq!(out[0][1], Value::Int(1), "the surviving A is row 1");
    assert_eq!(out[0][2], Value::Int(0), "B never survived");
    assert_eq!(out[0][3], Value::Int(1), "C matched row 2");
}

/// The same unwind, but through a SUBSET — a bound row is recorded under both its
/// own label and the union's, and both lists have to be truncated together.
///
/// `U` deliberately covers only `A` here. A union that also covered `B` would make
/// `U.v` inside `DEFINE[B]` read the candidate row itself (a row is bound before its
/// predicate runs, and a direct read wins over a `LAST` search), so `v > U.v` could
/// never hold and there would be no match to check.
#[test]
fn subset_lookups_survive_backtracking() {
    let out = run(
        "A+ B",
        r#"{"A":"v > 0","B":"v > U.v"}"#,
        r#"{"U":["A"]}"#,
        r#"{"n":"COUNT(*)","lastU":"FINAL LAST(U.v)","nu":"FINAL COUNT(U.*)"}"#,
        &[5, 4, 3, 9],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(4));
    // The last row U covers is the last A row — not the given-back one, and not B's.
    assert_eq!(out[0][1], Value::Int(3));
    assert_eq!(out[0][2], Value::Int(3), "U covers the three A rows");
}

/// Repeated groups re-bind the same labels many times over; the index must report
/// the most recent binding each time, not the first or a stale one.
#[test]
fn repeated_groups_track_the_most_recent_binding() {
    // Ascending pairs: A takes the lower value of each pair, B the higher.
    let out = run(
        "(A B)+",
        r#"{"A":"v > 0","B":"v > A.v"}"#,
        "",
        r#"{"n":"COUNT(*)","lastA":"FINAL LAST(A.v)","firstA":"FINAL FIRST(A.v)"}"#,
        &[1, 2, 3, 4, 5, 6],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(6));
    assert_eq!(out[0][1], Value::Int(5), "the last A row is the 5th");
    assert_eq!(out[0][2], Value::Int(1), "the first A row is the 1st");
}

/// A declared label that this match never binds reads as NULL — the index must not
/// confuse "no visible bind" with "not tracked". (A qualifier naming something the
/// pattern never declares is a bind error, so it cannot reach the evaluator.)
#[test]
fn unbound_qualifier_is_null() {
    let out = run(
        "A+ | B",
        // B is declared by the pattern but its predicate can never hold.
        r#"{"A":"v > 0","B":"v < 0"}"#,
        "",
        r#"{"b":"FINAL LAST(B.v)","a":"FINAL LAST(A.v)"}"#,
        &[7, 8],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Null, "B never binds");
    assert_eq!(out[0][1], Value::Int(8));
}

/// The point of the index: a long match with a qualified reference in its predicate
/// used to be quadratic. 40,000 rows would have been ~1.6 billion scan steps.
#[test]
fn long_match_with_a_qualified_reference_is_linear() {
    let n = 40_000usize;
    let vs: Vec<i64> = (1..=n as i64).collect();
    let out = run(
        "A B*",
        // Every row after the first compares against A, bound once at the start.
        r#"{"B":"v >= A.v"}"#,
        "",
        r#"{"n":"COUNT(*)","a":"FINAL LAST(A.v)"}"#,
        &vs,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(n as i64));
    assert_eq!(out[0][1], Value::Int(1));
}
