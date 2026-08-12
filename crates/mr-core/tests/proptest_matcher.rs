//! Property tests: on arbitrary patterns × arbitrary row tables the matcher
//! NEVER panics and ALWAYS terminates (within the step budget + the per-outer-
//! iteration tape-advance guarantee). When it produces output, the documented
//! invariants hold.

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;
use proptest::prelude::*;

struct Sch;
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        if name.eq_ignore_ascii_case("x") {
            Some(Ty::Int64)
        } else {
            None
        }
    }
    fn is_variable(&self, name: &str) -> bool {
        matches!(name.to_ascii_uppercase().as_str(), "A" | "B" | "C")
    }
}

/// A strategy producing (mostly) well-formed pattern strings over {A,B,C}.
fn pattern_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![Just("A"), Just("B"), Just("C")].prop_map(|s| s.to_string());
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            // quantifiers
            (
                inner.clone(),
                prop_oneof![Just("*"), Just("+"), Just("?"), Just("+?"), Just("{1,3}")]
            )
                .prop_map(|(p, q)| format!("({p}){q}")),
            // concatenation
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} {b}")),
            // alternation
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} | {b})")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, max_shrink_iters: 50, ..ProptestConfig::default() })]

    #[test]
    fn matcher_never_panics_and_terminates(
        pat in pattern_strategy(),
        data in prop::collection::vec(0i64..3, 0..12),
    ) {
        let cfg = PlanConfig {
            pattern: pat,
            // A,B,C distinguish by x; some rows match none.
            define_json: r#"{"A":"x = 0","B":"x = 1","C":"x = 2"}"#.into(),
            measures_json: Some(r#"{"mn":"MATCH_NUMBER()","n":"COUNT(*)"}"#.into()),
            partition_by: vec![],
            order_by: vec!["x".into()],
            rows_all: false,
            after: "past last row".into(),
            step_budget: Some(200_000),
        };
        let n = data.len();
        let store = VecRowStore::new(
            vec![("x", Ty::Int64)],
            data.into_iter().map(|v| vec![Value::Int(v)]).collect(),
        );
        // build may legitimately error (e.g. unbounded quantifier over nullable);
        // it must never panic.
        if let Ok(plan) = Plan::build(&cfg, &Sch) {
            match plan.run(&store) {
                Ok(rows) => {
                    // ONE ROW + past last row -> non-overlapping matches <= n.
                    prop_assert!(rows.len() <= n);
                    // MATCH_NUMBER strictly increases (single global partition).
                    let mut prev = 0i64;
                    for r in &rows {
                        if let Value::Int(m) = r[0] {
                            prop_assert!(m > prev, "match numbers must strictly increase");
                            prev = m;
                        }
                    }
                }
                Err(_) => { /* clean error (e.g. step budget) is acceptable */ }
            }
        }
    }
}

proptest! {
    // Fewer cases, far more rows: the generator above tops out at 12 rows, which
    // is why it never caught the stack overflow that long matches used to cause.
    // These partitions are past the old ~6k-row cliff, so an arbitrary pattern
    // over them must still either answer or fail cleanly — never abort.
    #![proptest_config(ProptestConfig { cases: 16, max_shrink_iters: 10, ..ProptestConfig::default() })]

    #[test]
    fn matcher_survives_large_partitions(
        pat in pattern_strategy(),
        // One dominant value across the whole partition, so an unbounded
        // quantifier really does bind every row in a *single* match. Mixed data
        // would sort into three short runs and never reach the interesting depth.
        (dominant, len) in (0i64..3, 12_000usize..20_000),
    ) {
        let data: Vec<i64> = std::iter::repeat_n(dominant, len).collect();
        let cfg = PlanConfig {
            pattern: pat,
            define_json: r#"{"A":"x = 0","B":"x = 1","C":"x = 2"}"#.into(),
            measures_json: Some(r#"{"mn":"MATCH_NUMBER()","n":"COUNT(*)"}"#.into()),
            partition_by: vec![],
            order_by: vec!["x".into()],
            rows_all: false,
            after: "past last row".into(),
            step_budget: Some(2_000_000),
        };
        let n = data.len();
        let store = VecRowStore::new(
            vec![("x", Ty::Int64)],
            data.into_iter().map(|v| vec![Value::Int(v)]).collect(),
        );
        if let Ok(plan) = Plan::build(&cfg, &Sch) {
            match plan.run(&store) {
                Ok(rows) => {
                    prop_assert!(rows.len() <= n);
                    // Each match reports at least one bound row (empty matches are
                    // omitted), and no match can exceed the partition.
                    for r in &rows {
                        if let Value::Int(cnt) = r[1] {
                            prop_assert!(cnt >= 1 && cnt <= n as i64,
                                "match row count {cnt} out of range for {n} rows");
                        }
                    }
                }
                Err(_) => { /* clean error (e.g. step budget) is acceptable */ }
            }
        }
    }
}
