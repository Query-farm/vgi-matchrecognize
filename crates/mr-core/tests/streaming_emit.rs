//! Streaming emission: `match_partition` + `emit_rows(limit)` must produce exactly
//! the rows `run` produces, whatever limit it is driven with.
//!
//! This is the contract the worker's producer relies on to bound its memory. The
//! interesting case is a limit *smaller than a match*, since under ALL ROWS PER
//! MATCH one match can be as long as the partition: the cursor has to resume with
//! the same RUNNING horizon, the same per-label bind index and the same aggregate
//! accumulators, or a resumed row differs from an uninterrupted one.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::rows::RowBuf;
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

/// `(pid BIGINT, ts BIGINT, v BIGINT)`, `n` rows over `parts` partitions.
fn store(parts: i64, n: i64) -> VecRowStore {
    VecRowStore::new(
        vec![("pid", Ty::Int64), ("ts", Ty::Int64), ("v", Ty::Int64)],
        (0..n)
            .map(|i| {
                vec![
                    Value::Int(i % parts),
                    Value::Int(i),
                    Value::Int((i * 7) % 13),
                ]
            })
            .collect(),
    )
}

fn plan(pattern: &str, define: &str, measures: &str, rows_all: bool, parts: bool) -> Plan {
    let cfg = PlanConfig {
        pattern: pattern.into(),
        define_json: define.into(),
        subset_json: String::new(),
        measures_json: Some(measures.into()),
        partition_by: if parts { vec!["pid".into()] } else { vec![] },
        include: Vec::new(),
        order_by: vec!["ts".into()],
        rows_all,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    let sch = Sch {
        cols: [
            ("pid".to_string(), Ty::Int64),
            ("ts".to_string(), Ty::Int64),
            ("v".to_string(), Ty::Int64),
        ]
        .into_iter()
        .collect(),
        labels: vec!["A".to_string(), "B".to_string()],
    };
    Plan::build(&cfg, &sch).expect("plan builds")
}

/// Drive the streaming API with a fixed batch limit, returning every row emitted
/// and the size of each batch.
fn streamed(plan: &Plan, store: &VecRowStore, limit: usize) -> (Vec<Vec<Value>>, Vec<usize>) {
    let mut parts = plan.partitions(store).expect("partitions");
    let mut out = Vec::new();
    let mut sizes = Vec::new();
    let mut buf = RowBuf::new(plan.output_columns().len());
    for p in 0..parts.len() {
        let mut run = {
            let (label, tape) = parts.part_mut(p);
            plan.match_partition(store, label, tape).expect("match")
        };
        while !run.is_done() {
            buf.clear();
            plan.emit_rows(store, parts.tape(p), &mut run, limit, &mut buf)
                .expect("emit");
            if buf.is_empty() {
                break;
            }
            sizes.push(buf.len());
            out.extend(buf.to_rows());
        }
    }
    (out, sizes)
}

/// Every limit must reproduce `run` exactly — same rows, same order.
fn agrees(pattern: &str, define: &str, measures: &str, rows_all: bool, parts: bool) {
    let store = store(if parts { 4 } else { 1 }, 200);
    let plan = plan(pattern, define, measures, rows_all, parts);
    let want = plan.run(&store).expect("run");
    for limit in [1, 2, 3, 7, 64, 199, 200, 1000] {
        let (got, sizes) = streamed(&plan, &store, limit);
        assert_eq!(got, want, "limit {limit} changed the result");
        assert!(
            sizes.iter().all(|&s| s <= limit || s == 0),
            "limit {limit} was overshot: {sizes:?}"
        );
    }
}

#[test]
fn one_row_per_match_streams_identically() {
    agrees(
        "A+",
        "{\"A\":\"v >= 0\"}",
        "{\"n\":\"COUNT(*)\"}",
        false,
        true,
    );
}

#[test]
fn all_rows_per_match_streams_identically() {
    agrees(
        "A+",
        "{\"A\":\"v >= 0\"}",
        "{\"n\":\"COUNT(*)\",\"cls\":\"CLASSIFIER()\"}",
        true,
        true,
    );
}

/// One match spanning the whole partition, split by every limit: the running
/// aggregate and the navigation both have to survive a batch boundary landing
/// inside the match.
#[test]
fn a_single_long_match_survives_being_split() {
    agrees(
        "A+",
        "{\"A\":\"v >= 0\"}",
        "{\"run\":\"SUM(v)\",\"fin\":\"FINAL SUM(v)\",\"prev\":\"PREV(v)\",\"first\":\"FIRST(v)\"}",
        true,
        false,
    );
}

/// Empty matches consume a match number and emit a row of their own, so the cursor
/// has to step over them the same way whatever the limit is.
#[test]
fn empty_matches_stream_identically() {
    agrees(
        "A*",
        "{\"A\":\"v > 100\"}",
        "{\"n\":\"COUNT(*)\",\"mn\":\"MATCH_NUMBER()\"}",
        true,
        true,
    );
}

/// Alternation with two labels, so `CLASSIFIER()` and a qualified reference are
/// both resolved from the resumed index rather than a freshly built one.
#[test]
fn qualified_references_survive_being_split() {
    agrees(
        "(A|B)+",
        "{\"A\":\"v % 2 = 0\",\"B\":\"v % 2 = 1\"}",
        "{\"la\":\"LAST(A.v)\",\"lb\":\"LAST(B.v)\",\"cls\":\"CLASSIFIER()\"}",
        true,
        true,
    );
}

/// A limit of zero must not spin: it emits nothing and leaves the cursor alone.
#[test]
fn a_zero_limit_makes_no_progress_and_does_not_hang() {
    let store = store(1, 32);
    let plan = plan(
        "A+",
        "{\"A\":\"v >= 0\"}",
        "{\"n\":\"COUNT(*)\"}",
        true,
        false,
    );
    let mut parts = plan.partitions(&store).expect("partitions");
    let mut run = {
        let (label, tape) = parts.part_mut(0);
        plan.match_partition(&store, label, tape).expect("match")
    };
    let mut buf = RowBuf::new(plan.output_columns().len());
    plan.emit_rows(&store, parts.tape(0), &mut run, 0, &mut buf)
        .expect("emit");
    assert_eq!(buf.len(), 0);
    assert!(!run.is_done(), "the cursor must still have rows to emit");
}
