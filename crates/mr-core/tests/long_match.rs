//! Regression tests for long matches (Blocker A).
//!
//! The matcher used to back-track with host recursion, so a single match spanning
//! more than roughly 6,000–8,000 rows overflowed the native stack and aborted the
//! process — the `A B*` sessionization and `A+` whole-partition shapes below are
//! exactly what triggered it. Backtracking now runs on an explicit heap stack, so
//! match length is bounded only by the step budget.
//!
//! These cases are deliberately far past the old cliff. They must complete and be
//! *correct*, not merely survive.

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

/// Single `x BIGINT` column; A/B/C are the pattern variables.
struct Sch;
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        name.eq_ignore_ascii_case("x").then_some(Ty::Int64)
    }
    fn is_variable(&self, name: &str) -> bool {
        matches!(name.to_ascii_uppercase().as_str(), "A" | "B" | "C")
    }
}

fn store_of(vals: impl IntoIterator<Item = i64>) -> VecRowStore {
    VecRowStore::new(
        vec![("x", Ty::Int64)],
        vals.into_iter().map(|v| vec![Value::Int(v)]).collect(),
    )
}

fn cfg(pattern: &str, define: &str, measures: &str, budget: Option<i64>) -> PlanConfig {
    PlanConfig {
        pattern: pattern.into(),
        define_json: define.into(),
        measures_json: Some(measures.into()),
        partition_by: vec![],
        order_by: vec!["x".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: budget,
    }
}

/// `A+` consuming a whole 200k-row partition in one match: 25× past the old
/// stack-overflow threshold.
#[test]
fn unbounded_quantifier_over_200k_rows_single_match() {
    const N: i64 = 200_000;
    let store = store_of(std::iter::repeat_n(1i64, N as usize));
    let plan = Plan::build(
        &cfg(
            "A+",
            r#"{"A":"x = 1"}"#,
            r#"{"mn":"MATCH_NUMBER()","n":"COUNT(*)"}"#,
            None,
        ),
        &Sch,
    )
    .unwrap();
    let out = plan.run(&store).unwrap();
    // Greedy `A+` swallows the whole partition, so exactly one match of N rows.
    assert_eq!(out.len(), 1, "one greedy match over the whole partition");
    assert_eq!(out[0][0], Value::Int(1), "match number");
    assert_eq!(out[0][1], Value::Int(N), "match spans every row");
}

/// The README's sessionization shape (`A B*` with a gap predicate) over a 100k-row
/// single session — the case that crashed the worker at 10k rows.
#[test]
fn sessionization_shape_over_100k_row_session() {
    const N: i64 = 100_000;
    // Consecutive x values: every row is within the "gap" of its predecessor, so
    // the whole partition is one session.
    let store = store_of(0..N);
    let plan = Plan::build(
        &cfg(
            "A B*",
            r#"{"B":"x <= PREV(x) + 1"}"#,
            r#"{"n":"COUNT(*)","first":"FIRST(x)","last":"LAST(x)"}"#,
            None,
        ),
        &Sch,
    )
    .unwrap();
    let out = plan.run(&store).unwrap();
    assert_eq!(out.len(), 1, "the whole partition is a single session");
    assert_eq!(out[0][0], Value::Int(N), "every row in the session");
    assert_eq!(out[0][1], Value::Int(0), "FIRST(x)");
    assert_eq!(out[0][2], Value::Int(N - 1), "LAST(x)");
}

/// A long match that must *backtrack* a long way: greedy `A+` overshoots the
/// whole partition, then gives rows back one at a time until the trailing `B`
/// can bind. Exercises deep unwinding of the heap stack, not just deep descent.
#[test]
fn deep_backtracking_over_50k_rows() {
    const N: i64 = 50_000;
    // 0..N-1 are A-eligible (x < N); the final row is the only B (x == N).
    let store = store_of((0..N).chain(std::iter::once(N)));
    let plan = Plan::build(
        &cfg(
            "A+ B",
            &format!(r#"{{"A":"x < {N}","B":"x = {N}"}}"#),
            r#"{"n":"COUNT(*)","nb":"COUNT(B.*)"}"#,
            None,
        ),
        &Sch,
    )
    .unwrap();
    let out = plan.run(&store).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], Value::Int(N + 1), "all A rows plus the B row");
    assert_eq!(out[0][1], Value::Int(1), "exactly one B");
}

/// A long match under ALL ROWS PER MATCH, which materializes one output row per
/// matched row (and so evaluates RUNNING measures 100k times).
#[test]
fn all_rows_per_match_over_100k_rows() {
    const N: i64 = 100_000;
    let mut c = cfg(
        "A+",
        r#"{"A":"x >= 0"}"#,
        r#"{"rn":"RUNNING COUNT(*)"}"#,
        None,
    );
    c.rows_all = true;
    let store = store_of(0..N);
    let plan = Plan::build(&c, &Sch).unwrap();
    let out = plan.run(&store).unwrap();
    assert_eq!(out.len(), N as usize, "one output row per matched row");
    // Columns: order_by (x), match_number, classifier, rn.
    assert_eq!(out[0][3], Value::Int(1), "RUNNING COUNT at the first row");
    assert_eq!(
        out[N as usize - 1][3],
        Value::Int(N),
        "RUNNING COUNT at the last row"
    );
}

/// The automatic budget scales with the partition, so an ordinary linear match
/// over a large partition is never cut off. A fixed default cannot do this: the
/// old constant 5,000,000 stopped a plain `A+` somewhere between 1M and 2M rows.
#[test]
fn automatic_budget_scales_with_partition_size() {
    use mr_core::plan::auto_step_budget;
    // Below the floor for small partitions, per-row past it.
    assert_eq!(
        auto_step_budget(0),
        5_000_000,
        "floor applies to empty input"
    );
    assert_eq!(
        auto_step_budget(1_000),
        5_000_000,
        "floor applies when small"
    );
    assert_eq!(auto_step_budget(1_000_000), 128_000_000, "scales per row");
    // No overflow on absurd inputs.
    assert!(auto_step_budget(usize::MAX) > 0);

    // The budget must exceed what a linear scan of the partition actually costs,
    // which is what makes an ordinary long match safe at any size. A greedy `A+`
    // over the whole partition is the cheapest such scan; verify it lands well
    // inside the allowance rather than merely passing.
    const N: usize = 200_000;
    let store = store_of(std::iter::repeat_n(1i64, N));
    let tight = Plan::build(
        &cfg(
            "A+",
            r#"{"A":"x = 1"}"#,
            r#"{"n":"COUNT(*)"}"#,
            // A tenth of the automatic allowance still suffices, so the default
            // has at least 10x headroom over a linear scan.
            Some(auto_step_budget(N) / 10),
        ),
        &Sch,
    )
    .unwrap();
    let out = tight.run(&store).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0][0],
        Value::Int(N as i64),
        "whole partition, one match"
    );
}

/// An explicit `step_budget` still pins the limit — the OS stack is never what
/// stops a runaway match, and it reports cleanly rather than crashing.
#[test]
fn step_budget_still_bounds_long_matches() {
    let store = store_of(std::iter::repeat_n(1i64, 100_000));
    let plan = Plan::build(
        &cfg("A+", r#"{"A":"x = 1"}"#, r#"{"n":"COUNT(*)"}"#, Some(5_000)),
        &Sch,
    )
    .unwrap();
    let err = plan.run(&store).expect_err("budget must be exhausted");
    assert!(
        err.to_string().contains("step budget"),
        "expected a clean step-budget error, got: {err}"
    );
}
