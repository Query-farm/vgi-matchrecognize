//! The Arrow store compares sort keys **in place**, in typed fast paths, while the
//! `RowStore` default materializes a [`Value`] and compares that. Two code paths
//! for one ordering is exactly the shape that drifts: a divergence would sort the
//! real worker differently from every pure-core test, silently.
//!
//! So this pins them together. For each supported key type it builds a column of
//! adversarial values — NULLs, duplicates, extremes, NaN, ±0.0 — and asserts that
//! `BatchRowStore::cmp_cells` returns exactly what the materializing comparison
//! returns, for every ordered pair and both NULL placements. It then sorts by the
//! column through both comparators and requires the same permutation.

use std::sync::Arc;

use arrow_array::{
    ArrayRef, Date32Array, Decimal128Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Schema};
use mr_core::engine::{valops, RowStore};
use mr_worker::arrow_in::BatchRowStore;

fn store_of(name: &str, array: ArrayRef) -> BatchRowStore {
    let schema = Arc::new(Schema::new(vec![Field::new(
        name,
        array.data_type().clone(),
        true,
    )]));
    let batch = RecordBatch::try_new(schema.clone(), vec![array]).unwrap();
    BatchRowStore::new(schema, vec![batch])
}

/// Every ordered pair must agree between the typed path and the materializing one.
fn assert_agrees(label: &str, store: &BatchRowStore) {
    let n = store.num_rows();
    for (desc, nulls_first) in [(false, false), (false, true), (true, false), (true, true)] {
        for a in 0..n {
            for b in 0..n {
                let typed = store.cmp_cells(a, b, 0, desc, nulls_first);
                let materialized =
                    valops::cmp_for_sort(&store.cell(a, 0), &store.cell(b, 0), desc, nulls_first);
                assert_eq!(
                    typed, materialized,
                    "{label}: rows {a} vs {b} (desc={desc}, nulls_first={nulls_first}) — typed path says \
                     {typed:?}, materializing path says {materialized:?}; values {:?} and {:?}",
                    store.cell(a, 0),
                    store.cell(b, 0)
                );
            }
        }
        // And the orderings they induce must be identical, not merely pairwise equal.
        let mut by_typed: Vec<usize> = (0..n).collect();
        by_typed.sort_by(|&a, &b| store.cmp_cells(a, b, 0, desc, nulls_first));
        let mut by_value: Vec<usize> = (0..n).collect();
        by_value.sort_by(|&a, &b| {
            valops::cmp_for_sort(&store.cell(a, 0), &store.cell(b, 0), desc, nulls_first)
        });
        assert_eq!(
            by_typed, by_value,
            "{label}: the two comparators produced different orderings \
             (desc={desc}, nulls_first={nulls_first})"
        );
    }
}

#[test]
fn int64_keys_agree() {
    let a: ArrayRef = Arc::new(Int64Array::from(vec![
        Some(0),
        None,
        Some(i64::MIN),
        Some(i64::MAX),
        Some(-1),
        Some(1),
        Some(0),
        None,
    ]));
    assert_agrees("Int64", &store_of("k", a));
}

#[test]
fn int32_and_date32_keys_agree() {
    let a: ArrayRef = Arc::new(Int32Array::from(vec![
        Some(7),
        None,
        Some(i32::MIN),
        Some(i32::MAX),
        Some(7),
    ]));
    assert_agrees("Int32", &store_of("k", a));

    let d: ArrayRef = Arc::new(Date32Array::from(vec![
        Some(0),
        None,
        Some(-19_000),
        Some(19_000),
        Some(0),
    ]));
    assert_agrees("Date32", &store_of("k", d));
}

/// The interesting one: `total_cmp` orders NaN and distinguishes ±0.0, while SQL
/// comparison leaves NaN unordered. Sorting has to pick a total order, and both
/// paths must pick the *same* one.
#[test]
fn float64_keys_agree_including_nan_and_signed_zero() {
    let a: ArrayRef = Arc::new(Float64Array::from(vec![
        Some(1.5),
        None,
        Some(f64::NAN),
        Some(-0.0),
        Some(0.0),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(1.5),
        Some(-f64::NAN),
    ]));
    assert_agrees("Float64", &store_of("k", a));
}

#[test]
fn utf8_keys_agree() {
    let a: ArrayRef = Arc::new(StringArray::from(vec![
        Some("b"),
        None,
        Some(""),
        Some("a"),
        Some("B"),
        Some("b"),
        Some("ábc"),
        Some("abc"),
    ]));
    assert_agrees("Utf8", &store_of("k", a));
}

#[test]
fn timestamp_keys_agree() {
    let a: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![
        Some(0),
        None,
        Some(-1),
        Some(i64::MAX / 2),
        Some(0),
    ]));
    assert_agrees("Timestamp(us)", &store_of("k", a));
}

/// Decimals compare exactly as unscaled integers in the typed path; the
/// materializing path must not fall back to lossy `f64` for values past 2^53.
#[test]
fn decimal_keys_agree_beyond_f64_precision() {
    let big = 1i128 << 60;
    let a: ArrayRef = Arc::new(
        Decimal128Array::from(vec![
            Some(big),
            None,
            Some(big + 1),
            Some(-big),
            Some(0),
            Some(big),
        ])
        .with_precision_and_scale(38, 0)
        .unwrap(),
    );
    assert_agrees("Decimal128(38,0)", &store_of("k", a));
}

/// A column spread across several buffered batches still compares correctly: the
/// typed path has to locate each row's batch before touching a buffer.
#[test]
fn keys_agree_across_batch_boundaries() {
    let schema = Arc::new(Schema::new(vec![Field::new("k", DataType::Int64, true)]));
    let batches: Vec<RecordBatch> = [vec![Some(5), None], vec![Some(1)], vec![Some(9), Some(5)]]
        .into_iter()
        .map(|v| {
            RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int64Array::from(v)) as ArrayRef],
            )
            .unwrap()
        })
        .collect();
    let store = BatchRowStore::new(schema, batches);
    assert_eq!(store.num_rows(), 5);
    assert_agrees("Int64 across batches", &store);
}
