//! Sorting and partitioning at scale, and on inputs designed to break a sort.
//!
//! Ordering is load-bearing in a way it is not for most operators: the matcher
//! walks the partition in tape order, so a mis-sorted tape does not produce
//! slightly-off output, it produces *different matches*. And the failure is quiet —
//! there is nothing in the result that looks wrong. So these tests check the tape
//! order itself, against an independently computed reference ordering.
//!
//! The tape is observed through the public API: `ALL ROWS PER MATCH` over a pattern
//! that binds every row emits one output row per input row, in tape order.

use std::collections::HashMap;

use mr_core::engine::VecRowStore;
use mr_core::plan::{Plan, PlanConfig};
use mr_core::types::{BindSchema, Ty};
use mr_core::value::Value;

struct Sch {
    cols: HashMap<String, Ty>,
}
impl BindSchema for Sch {
    fn col_ty(&self, name: &str) -> Option<Ty> {
        self.cols
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, t)| t.clone())
    }
    fn is_variable(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("A")
    }
}

fn sch(cols: &[(&str, Ty)]) -> Sch {
    Sch {
        cols: cols
            .iter()
            .map(|(n, t)| (n.to_string(), t.clone()))
            .collect(),
    }
}

/// Run with `ALL ROWS PER MATCH` over a pattern that binds everything, so the
/// output is one row per input row **in tape order**.
fn tape_order(
    headers: &[(&str, Ty)],
    rows: Vec<Vec<Value>>,
    partition_by: &[&str],
    order_by: &[&str],
) -> Vec<Vec<Value>> {
    tape_order_with(
        headers,
        rows,
        partition_by,
        order_by,
        r#"{"pos":"RUNNING COUNT(*)"}"#,
    )
}

/// As above, with an explicit `measures` object when a test needs to observe a
/// column that the ALL ROWS layout does not emit.
fn tape_order_with(
    headers: &[(&str, Ty)],
    rows: Vec<Vec<Value>>,
    partition_by: &[&str],
    order_by: &[&str],
    measures: &str,
) -> Vec<Vec<Value>> {
    let cfg = PlanConfig {
        include: Vec::new(),
        pattern: "A+".into(),
        define_json: "{}".into(), // undefined variable: binds any row
        subset_json: String::new(),
        measures_json: Some(measures.to_string()),
        partition_by: partition_by.iter().map(|s| s.to_string()).collect(),
        order_by: order_by.iter().map(|s| s.to_string()).collect(),
        rows_all: true,
        omit_empty_matches: false,
        after: "past last row".into(),
        step_budget: None,
    };
    let store = VecRowStore::new(headers.to_vec(), rows);
    Plan::build(&cfg, &sch(headers))
        .unwrap()
        .run(&store)
        .unwrap()
}

/// Deterministic scramble, so the sort never sees pre-sorted input by accident.
fn scramble(i: usize, n: usize) -> i64 {
    ((i.wrapping_mul(2_654_435_761)) % n) as i64
}

// ------------------------------------------------------------------ scale

/// The order-by column must come out non-decreasing for every row, at a size where
/// the sort is doing real work (and where an O(n^2) mistake would be obvious).
#[test]
fn sorts_a_large_partition_correctly() {
    const N: usize = 50_000;
    let headers = [("k", Ty::Int64)];
    let rows: Vec<Vec<Value>> = (0..N).map(|i| vec![Value::Int(scramble(i, N))]).collect();
    let out = tape_order(&headers, rows, &[], &["k"]);
    assert_eq!(out.len(), N, "one output row per input row");
    // Columns: k (order_by), match_number, classifier, pos.
    let mut prev = i64::MIN;
    for (i, r) in out.iter().enumerate() {
        let Value::Int(k) = r[0] else {
            panic!("expected an integer key at row {i}")
        };
        assert!(k >= prev, "tape not ordered at row {i}: {k} after {prev}");
        prev = k;
    }
    // RUNNING COUNT(*) must be 1..N: the whole partition is one match, so this
    // also proves no row was dropped or emitted twice.
    for (i, r) in out.iter().enumerate() {
        assert_eq!(r[3], Value::Int(i as i64 + 1), "position at row {i}");
    }
}

/// Many partitions and a large partition in the same input: the grouping map and
/// the per-partition sort both have to hold up.
#[test]
fn many_partitions_plus_one_large_partition() {
    const SMALL: usize = 20_000; // 20k partitions of 1 row
    const BIG: usize = 30_000; // plus one partition of 30k rows
    let headers = [("pid", Ty::Int64), ("k", Ty::Int64)];
    let mut rows: Vec<Vec<Value>> = (0..SMALL)
        .map(|i| vec![Value::Int(i as i64 + 1), Value::Int(0)])
        .collect();
    rows.extend((0..BIG).map(|i| vec![Value::Int(0), Value::Int(scramble(i, BIG))]));
    let out = tape_order(&headers, rows, &["pid"], &["k"]);
    assert_eq!(out.len(), SMALL + BIG);
    // The big partition's rows must be internally ordered. Partition 0 is emitted
    // first only if it is seen first; find its rows by pid instead of assuming.
    let big: Vec<i64> = out
        .iter()
        .filter(|r| r[0] == Value::Int(0))
        .map(|r| match r[1] {
            Value::Int(k) => k,
            _ => panic!("expected an integer key"),
        })
        .collect();
    assert_eq!(big.len(), BIG, "the large partition kept all its rows");
    assert!(
        big.windows(2).all(|w| w[0] <= w[1]),
        "the large partition is not ordered"
    );
}

// ------------------------------------------------- adversarial distributions

#[test]
fn handles_adversarial_key_distributions() {
    const N: usize = 20_000;
    let headers = [("k", Ty::Int64)];
    let cases: Vec<(&str, Vec<i64>)> = vec![
        ("already ascending", (0..N as i64).collect()),
        ("already descending", (0..N as i64).rev().collect()),
        ("all equal", vec![7; N]),
        (
            "two distinct values",
            (0..N).map(|i| (i % 2) as i64).collect(),
        ),
        (
            "sawtooth",
            (0..N).map(|i| (i % 100) as i64).collect::<Vec<_>>(),
        ),
        (
            "organ pipe",
            (0..N)
                .map(|i| if i < N / 2 { i } else { N - i } as i64)
                .collect(),
        ),
    ];
    for (label, keys) in cases {
        let rows: Vec<Vec<Value>> = keys.iter().map(|k| vec![Value::Int(*k)]).collect();
        let out = tape_order(&headers, rows, &[], &["k"]);
        assert_eq!(out.len(), keys.len(), "{label}: row count");
        let got: Vec<i64> = out
            .iter()
            .map(|r| match r[0] {
                Value::Int(k) => k,
                _ => panic!("{label}: expected an integer key"),
            })
            .collect();
        let mut want = keys.clone();
        want.sort_unstable();
        assert_eq!(got, want, "{label}: wrong order");
    }
}

/// DESC, NULLS FIRST and NULLS LAST, against a reference ordering.
#[test]
fn direction_and_null_placement() {
    const N: usize = 5_000;
    let headers = [("k", Ty::Int64)];
    // A third of the rows are NULL, interleaved.
    let keys: Vec<Option<i64>> = (0..N)
        .map(|i| {
            if i % 3 == 0 {
                None
            } else {
                Some(scramble(i, N))
            }
        })
        .collect();
    let rows: Vec<Vec<Value>> = keys
        .iter()
        .map(|k| vec![k.map(Value::Int).unwrap_or(Value::Null)])
        .collect();

    for (spec, desc, nulls_first) in [
        ("k", false, false),
        ("k DESC", true, false),
        ("k NULLS FIRST", false, true),
        ("k DESC NULLS FIRST", true, true),
    ] {
        let out = tape_order(&headers, rows.clone(), &[], &[spec]);
        assert_eq!(out.len(), N, "{spec}: row count");
        let got: Vec<Option<i64>> = out
            .iter()
            .map(|r| match r[0] {
                Value::Int(k) => Some(k),
                Value::Null => None,
                ref other => panic!("{spec}: unexpected {other:?}"),
            })
            .collect();

        // Reference: sort non-NULLs, then place NULLs at the requested end.
        let mut present: Vec<i64> = keys.iter().flatten().copied().collect();
        present.sort_unstable();
        if desc {
            present.reverse();
        }
        let nulls = keys.len() - present.len();
        let mut want: Vec<Option<i64>> = Vec::with_capacity(keys.len());
        if nulls_first {
            want.extend(std::iter::repeat_n(None, nulls));
        }
        want.extend(present.into_iter().map(Some));
        if !nulls_first {
            want.extend(std::iter::repeat_n(None, nulls));
        }
        assert_eq!(got, want, "{spec}: wrong order");
    }
}

/// A second key must break ties in the first, and only there.
#[test]
fn multi_column_order_by() {
    const N: usize = 10_000;
    let headers = [("a", Ty::Int64), ("b", Ty::Int64)];
    // `a` has only 10 distinct values, so `b` decides most of the ordering.
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| {
            vec![
                Value::Int(scramble(i, N) % 10),
                Value::Int(scramble(i * 7 + 1, N)),
            ]
        })
        .collect();
    let mut want: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            _ => unreachable!(),
        })
        .collect();
    want.sort_unstable_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1))); // a ASC, b DESC

    let out = tape_order(&headers, rows, &[], &["a", "b DESC"]);
    let got: Vec<(i64, i64)> = out
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got, want);
}

/// A VARCHAR key exercises the comparison path that used to allocate per
/// comparison, and collation matters: ordering must be by bytes, consistently.
#[test]
fn varchar_keys_sort_consistently() {
    const N: usize = 20_000;
    let headers = [("s", Ty::Varchar)];
    let keys: Vec<String> = (0..N)
        .map(|i| format!("key-{:08}", scramble(i, N)))
        .collect();
    let rows: Vec<Vec<Value>> = keys.iter().map(|s| vec![Value::Str(s.clone())]).collect();
    let out = tape_order(&headers, rows, &[], &["s"]);
    let got: Vec<String> = out
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect();
    let mut want = keys;
    want.sort_unstable();
    assert_eq!(got, want);
}

/// A NaN in the sort key must not scramble the partition. `partial_cmp` maps NaN
/// pairs to "unordered", and treating that as Equal is intransitive — a comparator
/// like that lets `sort_by` return an arbitrary permutation, so the whole tape
/// (not just the NaN row) becomes unpredictable. Sorting uses a total order.
#[test]
fn nan_in_the_sort_key_keeps_a_total_order() {
    const N: usize = 2_000;
    let headers = [("k", Ty::Double)];
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| {
            vec![Value::Double(if i % 97 == 0 {
                f64::NAN
            } else {
                scramble(i, N) as f64
            })]
        })
        .collect();
    let out = tape_order(&headers, rows, &[], &["k"]);
    assert_eq!(out.len(), N);
    let got: Vec<f64> = out
        .iter()
        .map(|r| match r[0] {
            Value::Double(d) => d,
            ref other => panic!("expected a double, got {other:?}"),
        })
        .collect();
    // Non-NaN values must still be ordered among themselves, and every NaN must
    // sit at one end (total_cmp puts positive NaN last).
    let non_nan: Vec<f64> = got.iter().copied().filter(|d| !d.is_nan()).collect();
    assert!(
        non_nan.windows(2).all(|w| w[0] <= w[1]),
        "non-NaN keys are not ordered: the comparator is not a total order"
    );
    let nan_count = got.iter().filter(|d| d.is_nan()).count();
    assert_eq!(nan_count, (0..N).filter(|i| i % 97 == 0).count());
    assert!(
        got.iter().rev().take(nan_count).all(|d| d.is_nan()),
        "NaNs should be grouped at one end"
    );
}

/// Timestamps far past the microsecond overflow point must still order correctly:
/// rescaling i64 ticks by the unit factor wrapped, and `TIMESTAMP '9999-12-31'`
/// sorted *before* 2020, silently reversing the tape.
#[test]
fn far_future_timestamps_order_correctly() {
    use mr_core::types::TimeUnit;
    let headers = [("ts", Ty::Timestamp(TimeUnit::Micro))];
    // Microseconds since the epoch: 2020, 2026, then year 9999.
    let ticks: Vec<i64> = vec![
        1_577_836_800_000_000,
        1_767_225_600_000_000,
        253_402_214_400_000_000,
        -1_000_000, // just before the epoch
    ];
    let rows: Vec<Vec<Value>> = ticks
        .iter()
        .map(|t| vec![Value::Timestamp(*t, TimeUnit::Micro)])
        .collect();
    let out = tape_order(&headers, rows, &[], &["ts"]);
    let got: Vec<i64> = out
        .iter()
        .map(|r| match r[0] {
            Value::Timestamp(t, _) => t,
            ref other => panic!("expected a timestamp, got {other:?}"),
        })
        .collect();
    let mut want = ticks;
    want.sort_unstable();
    assert_eq!(got, want, "far-future timestamps must not wrap");
}

/// Equal keys must keep their input order: `AFTER MATCH SKIP` and the navigation
/// functions are all defined in terms of tape position, so an unstable sort would
/// make results depend on the sort's internals.
#[test]
fn ties_preserve_input_order() {
    const N: usize = 10_000;
    let headers = [("k", Ty::Int64), ("seq", Ty::Int64)];
    // Two distinct keys, so almost every pair is a tie.
    let rows: Vec<Vec<Value>> = (0..N)
        .map(|i| vec![Value::Int((i % 2) as i64), Value::Int(i as i64)])
        .collect();
    // `seq` is not an order or partition column, so project it through a measure.
    // Columns: k (order_by), match_number, classifier, seq.
    let out = tape_order_with(&headers, rows, &[], &["k"], r#"{"seq":"LAST(seq)"}"#);
    // Within each key group, `seq` must still ascend.
    for key in [0i64, 1] {
        let seqs: Vec<i64> = out
            .iter()
            .filter(|r| r[0] == Value::Int(key))
            .map(|r| match r[3] {
                Value::Int(s) => s,
                ref other => panic!("expected seq, got {other:?}"),
            })
            .collect();
        assert_eq!(seqs.len(), N / 2, "key {key}: group size");
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "key {key}: ties did not preserve input order"
        );
    }
}
