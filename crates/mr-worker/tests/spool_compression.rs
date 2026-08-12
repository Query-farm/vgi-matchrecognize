//! Spool compression: off unless asked for, and correct when it is.
//!
//! `VGI_MR_SPOOL_COMPRESSION=lz4` is opt-in — see `spool::compression` for why it is not
//! the default (it makes the on-disk size stop predicting the producer's in-memory peak,
//! which is what the shard count is derived from). Without a test the path would rot.
//!
//! Both tests live in one binary and set a process-wide environment variable, so they
//! must not run concurrently with each other; `cargo test` runs the tests of one binary
//! on separate threads, hence the mutex.

use std::sync::{Arc, Mutex};

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use mr_worker::spool;

/// Serialises the two tests, which share `VGI_MR_SPOOL_COMPRESSION`.
static ENV: Mutex<()> = Mutex::new(());

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

/// Asked for, a spool leaves the first 32 MB plain and compresses the rest — so one file
/// holds records of both codecs, and every record must still come back.
#[test]
fn lz4_compresses_past_the_threshold_and_round_trips() {
    let _guard = ENV.lock().unwrap();
    const COLS: usize = 8;
    const ROWS: usize = 2048;
    // 8 columns x 2048 rows x 8 bytes is ~128 KB per batch, so this comfortably passes
    // the 32 MB switch point.
    const BATCHES: i64 = 320;

    std::env::set_var("VGI_MR_SPOOL_COMPRESSION", "lz4");
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

    let bytes = spool::sink_bytes(&scope);
    let plain = (BATCHES as usize * ROWS * COLS * 8) as u64;
    assert!(
        bytes > 32 * 1024 * 1024,
        "the first 32 MB should be uncompressed, but the whole spool is {bytes} bytes"
    );
    assert!(
        bytes < plain,
        "the tail should be compressed: {bytes} bytes against a plain estimate of {plain}"
    );
    spool::discard(&scope);
    std::env::remove_var("VGI_MR_SPOOL_COMPRESSION");
}

/// Unasked, nothing is compressed — however much is written, and whatever it looks like.
/// This is the property the shard count depends on: bytes on disk predict what the
/// producer will hold in memory.
#[test]
fn plain_by_default_and_the_reader_needs_no_setting() {
    let _guard = ENV.lock().unwrap();
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
    // Highly compressible data, left alone: the spool is at least the size of the raw
    // string column, which no compressed encoding of it would be.
    let bytes = spool::sink_bytes(&scope);
    assert!(
        bytes > 8 * 2048 * 25,
        "expected uncompressed output, got {bytes} bytes"
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
