//! Spool compression: on past 32 MB, off below it, and correct across the boundary.
//!
//! The threshold means a short query is never compressed and a large one mostly is, so
//! one file routinely holds records of both codecs — which is the case worth testing.
//! `VGI_MR_SPOOL_COMPRESSION=lz4|none` forces either way.
//!
//! Both tests live in one binary and set a process-wide environment variable, so they
//! must not run concurrently with each other; `cargo test` runs the tests of one binary
//! on separate threads, hence the mutex.

use std::sync::{Arc, Mutex};

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use mr_worker::spool;

/// Serialises the two tests, which share `VGI_MR_SPOOL_COMPRESSION`. Poisoning is
/// ignored, so a failure in one test reports its own assertion rather than turning the
/// other into a confusing `PoisonError`.
static ENV: Mutex<()> = Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the spool actually occupies, as opposed to what it accounts for.
///
/// `sink_uncompressed_bytes` deliberately reports the *uncompressed* total, because that
/// is what the shard count must be derived from — so proving that compression happened
/// means comparing the two.
fn on_disk(scope: &[u8]) -> u64 {
    spool::files_with_prefix(scope, "sink-")
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

fn wide_schema(cols: usize) -> SchemaRef {
    Arc::new(Schema::new(
        (0..cols)
            .map(|c| Field::new(format!("c{c}"), DataType::Int64, true))
            .collect::<Vec<_>>(),
    ))
}

/// A batch of `cols` int64 columns, repetitive so that compressing it visibly pays.
fn wide_batch(schema: &SchemaRef, cols: usize, rows: usize) -> RecordBatch {
    let columns: Vec<ArrayRef> = (0..cols)
        .map(|c| {
            Arc::new(Int64Array::from(
                (0..rows as i64)
                    .map(|r| (r + c as i64) % 8)
                    .collect::<Vec<_>>(),
            )) as ArrayRef
        })
        .collect();
    RecordBatch::try_new(schema.clone(), columns).unwrap()
}

/// A spool past the threshold holds records of both codecs, and every one must come back.
#[test]
fn lz4_compresses_past_the_threshold_and_round_trips() {
    let _guard = env_lock();
    const COLS: usize = 8;
    const ROWS: usize = 2048;
    // 8 columns x 2048 rows x 8 bytes is ~128 KB per batch, so this comfortably passes
    // the 32 MB switch point.
    const BATCHES: i64 = 320;

    // The default would do this anyway at this size; unset it so the *default* is what is
    // under test rather than the override.
    std::env::remove_var("VGI_MR_SPOOL_COMPRESSION");
    let schema = wide_schema(COLS);
    let scope = format!("lz4-mixed-{}", std::process::id()).into_bytes();
    for b in 0..BATCHES {
        assert!(
            spool::append(&scope, &wide_batch(&schema, COLS, ROWS), Some(b)).unwrap(),
            "the spool must be available for this test"
        );
    }

    let mut back = spool::read_all(&scope).unwrap();
    back.sort_by_key(|(i, _)| *i);
    assert_eq!(back.len(), BATCHES as usize, "every batch must read back");
    assert_eq!(
        back.iter().map(|(_, b)| b.num_rows()).sum::<usize>(),
        BATCHES as usize * ROWS,
        "no rows lost across the codec switch"
    );
    // Values survive, at both ends of the file — so the plain records and the compressed
    // ones both decode.
    for (_, batch) in [&back[0], &back[BATCHES as usize - 1]] {
        let c0 = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(c0.value(0), 0);
        assert_eq!(c0.value(9), 1);
    }

    // The accounting is uncompressed by construction; the files are not. One being
    // smaller than the other is exactly the evidence that the codec engaged.
    let accounted = spool::sink_uncompressed_bytes(&scope);
    let stored = on_disk(&scope);
    assert!(
        stored < accounted,
        "the tail should be compressed: {stored} bytes stored against {accounted} accounted"
    );
    assert!(
        stored > 32 * 1024 * 1024,
        "the first 32 MB should be uncompressed, but only {stored} bytes were stored"
    );
    // And the accounting must still describe the real data, since the memory budget is
    // derived from it: 8 columns x 8 bytes x rows, plus framing.
    let values = (BATCHES as usize * ROWS * COLS * 8) as u64;
    assert!(
        accounted >= values && accounted < values * 2,
        "accounted {accounted} should be the uncompressed size of {values} bytes of values"
    );
    spool::discard(&scope);
}

/// Below the threshold nothing is compressed, so a short query pays no CPU for it.
#[test]
fn small_spools_are_left_plain() {
    let _guard = env_lock();
    std::env::remove_var("VGI_MR_SPOOL_COMPRESSION");

    let schema = Arc::new(Schema::new(vec![
        Field::new("v", DataType::Int64, true),
        Field::new("s", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(
                (0..2048i64).map(|i| i % 4).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(StringArray::from(
                (0..2048)
                    .map(|_| "the same string every row")
                    .collect::<Vec<_>>(),
            )) as ArrayRef,
        ],
    )
    .unwrap();

    let scope = format!("plain-{}", std::process::id()).into_bytes();
    for b in 0..8i64 {
        assert!(spool::append(&scope, &batch, Some(b)).unwrap());
    }
    // Highly compressible data, left alone. With nothing compressed each record occupies
    // its payload plus a 64-byte header plus at most 63 bytes of padding to the next
    // 64-byte boundary — and nothing *less*, which is what rules out a codec having
    // quietly engaged.
    let (stored, accounted) = (on_disk(&scope), spool::sink_uncompressed_bytes(&scope));
    const RECORDS: u64 = 8;
    assert!(
        stored >= accounted + RECORDS * 64 && stored < accounted + RECORDS * 128,
        "expected {accounted} accounted bytes plus a header and padding per record, got \
         {stored} stored"
    );

    let back = spool::read_all(&scope).unwrap();
    assert_eq!(back.len(), 8);
    let (_, first) = &back[0];
    let s = first
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("utf8");
    assert_eq!(s.value(0), "the same string every row");
    spool::discard(&scope);
}
