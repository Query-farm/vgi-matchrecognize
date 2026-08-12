//! Splitting the buffered relation by partition key.
//!
//! When the input is larger than the finalize memory budget, `combine` rewrites the
//! spool into shards so each producer holds a shard rather than the whole relation.
//! That rests on one property: **every row of a partition lands in the same shard**.
//! If it did not, a partition would be matched twice over disjoint halves and the
//! answer would be wrong in a way no row count could reveal — so this test asserts it
//! directly, rather than only through a query's output.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_worker::{shard, spool};

struct Sch;
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        match name.to_ascii_lowercase().as_str() {
            "uid" | "ts" => Some(Ty::Int64),
            _ => None,
        }
    }
    fn is_variable(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("A")
    }
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("uid", DataType::Int64, true),
        Field::new("ts", DataType::Int64, true),
    ]))
}

fn plan() -> Plan {
    let cfg = PlanConfig {
        pattern: "A+".into(),
        define_json: "{}".into(),
        subset_json: String::new(),
        measures_json: Some(r#"{"n":"COUNT(*)"}"#.into()),
        partition_by: vec!["uid".into()],
        order_by: vec!["ts".into()],
        rows_all: false,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    Plan::build(&cfg, &Sch).unwrap()
}

/// A batch of `(uid, ts)` rows, uid cycling through `partitions`.
fn batch(from: i64, n: i64, partitions: i64) -> RecordBatch {
    let uid: Vec<i64> = (from..from + n).map(|i| i % partitions).collect();
    let ts: Vec<i64> = (from..from + n).collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(uid)) as ArrayRef,
            Arc::new(Int64Array::from(ts)) as ArrayRef,
        ],
    )
    .unwrap()
}

#[test]
fn every_partition_lands_in_exactly_one_shard() {
    const PARTITIONS: i64 = 97;
    const BATCHES: i64 = 20;
    const PER_BATCH: i64 = 500;
    const SHARDS: usize = 8;

    let scope = format!("shard-split-{}", std::process::id()).into_bytes();
    let plan = plan();
    let schema = schema();

    for b in 0..BATCHES {
        assert!(
            spool::append(
                &scope,
                &batch(b * PER_BATCH, PER_BATCH, PARTITIONS),
                Some(b)
            )
            .unwrap(),
            "the spool must be available for this test"
        );
    }
    let total_rows = (BATCHES * PER_BATCH) as usize;

    let rows_per_shard = shard::split(&scope, &plan, &schema, SHARDS).unwrap();
    assert_eq!(rows_per_shard.len(), SHARDS);
    assert_eq!(
        rows_per_shard.iter().sum::<u64>() as usize,
        total_rows,
        "the split must account for every row"
    );
    // The unsharded files are consumed, so nothing can be read twice.
    assert!(
        spool::read_all(&scope).unwrap().is_empty(),
        "sink files should be removed once their rows are in shards"
    );

    // Where each partition ended up, and how many of its rows arrived.
    let mut shard_of: HashMap<i64, usize> = HashMap::new();
    let mut rows_of: HashMap<i64, usize> = HashMap::new();
    let mut seen_ts: HashSet<i64> = HashSet::new();
    for (s, expect) in rows_per_shard.iter().enumerate() {
        let batches = spool::read_shard(&scope, s).unwrap();
        let rows: usize = batches.iter().map(|(_, b)| b.num_rows()).sum();
        assert_eq!(
            rows as u64, *expect,
            "shard {s} holds a different number of rows than the split reported"
        );
        for (_, b) in &batches {
            let uid = b
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("uid is Int64");
            let ts = b
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("ts is Int64");
            for i in 0..b.num_rows() {
                let u = uid.value(i);
                // THE invariant: a partition may not appear in two shards.
                let prev = shard_of.entry(u).or_insert(s);
                assert_eq!(
                    *prev, s,
                    "partition {u} appears in shards {prev} and {s}; a partition split across \
                     shards would be matched twice over disjoint halves"
                );
                *rows_of.entry(u).or_insert(0) += 1;
                assert!(
                    seen_ts.insert(ts.value(i)),
                    "row {} was duplicated",
                    ts.value(i)
                );
            }
        }
    }

    assert_eq!(
        seen_ts.len(),
        total_rows,
        "every row must survive the split exactly once"
    );
    assert_eq!(
        shard_of.len(),
        PARTITIONS as usize,
        "every partition should be present"
    );
    for (uid, rows) in &rows_of {
        assert_eq!(
            *rows,
            total_rows / PARTITIONS as usize
                + usize::from((*uid as usize) < total_rows % PARTITIONS as usize),
            "partition {uid} lost or gained rows"
        );
    }
    // More than one shard should actually be used, or the test proves nothing.
    let used = rows_per_shard.iter().filter(|r| **r > 0).count();
    assert!(
        used > 1,
        "expected the split to use several shards, used {used}"
    );

    for s in 0..SHARDS {
        spool::discard_shard(&scope, s);
    }
    spool::discard(&scope);
}

/// A single shard is a no-op the caller is expected to avoid; the decision function is
/// what keeps it from happening.
#[test]
fn shard_count_respects_budget_and_shape() {
    let budget = shard::budget_bytes();
    // Small input: one shard, no rewrite.
    assert_eq!(shard::shard_count(budget / 2, true), 1);
    assert_eq!(shard::shard_count(0, true), 1);
    // Over budget: enough shards to bring each under it.
    assert_eq!(shard::shard_count(budget * 2, true), 2);
    assert_eq!(shard::shard_count(budget * 2 + 1, true), 3);
    // Unpartitioned input is one partition, so no split can divide it.
    assert_eq!(shard::shard_count(budget * 100, false), 1);
    // Capped, so a huge input does not mean unbounded files and streams.
    assert!(shard::shard_count(budget * 10_000, true) <= 64);
}
