//! The compressed spool path round-trips.
//!
//! `VGI_MR_SPOOL_COMPRESSION=lz4` is off by default (see `spool::compression` for the
//! measurements that decided that), so without a test the path would rot unnoticed.
//! One test in its own binary, because it sets a process-wide environment variable.
//!
//! The second test covers the case the default actually produces: a sink starts writing
//! plain and switches to LZ4 once it has written enough for the bytes to matter, so one
//! file holds records of *both* codecs and must read back as one relation.

use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use mr_worker::spool;

#[test]
fn lz4_spool_round_trips_and_is_smaller() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("v", DataType::Int64, true),
        Field::new("s", DataType::Utf8, true),
    ]));
    // Repetitive on purpose: this asserts the codec is actually engaged, and only
    // compressible data can show that in the file size.
    let batch = |from: i64| {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(
                    (from..from + 2048).map(|i| i % 8).collect::<Vec<_>>(),
                )) as ArrayRef,
                Arc::new(StringArray::from(
                    (from..from + 2048)
                        .map(|_| "the same string every row")
                        .collect::<Vec<_>>(),
                )) as ArrayRef,
            ],
        )
        .unwrap()
    };

    let mut sizes = Vec::new();
    for mode in ["none", "lz4"] {
        std::env::set_var("VGI_MR_SPOOL_COMPRESSION", mode);
        let scope = format!("compress-{mode}-{}", std::process::id()).into_bytes();
        for b in 0..4i64 {
            assert!(spool::append(&scope, &batch(b * 2048), Some(b)).unwrap());
        }
        sizes.push(spool::sink_bytes(&scope));

        // The reader is mode-agnostic: it decompresses whatever the writer chose, which
        // is what lets a spool written before the knob changed still be read.
        let mut back = spool::read_all(&scope).unwrap();
        back.sort_by_key(|(i, _)| *i);
        assert_eq!(back.len(), 4, "{mode}: every batch must come back");
        assert_eq!(
            back.iter().map(|(_, b)| b.num_rows()).sum::<usize>(),
            4 * 2048,
            "{mode}: no rows lost"
        );
        let first = &back[0].1;
        let v = first
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let s = first
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(v.value(0), 0);
        assert_eq!(v.value(9), 1, "{mode}: values survive the round trip");
        assert_eq!(s.value(0), "the same string every row");
        spool::discard(&scope);
    }
    std::env::remove_var("VGI_MR_SPOOL_COMPRESSION");

    let (plain, lz4) = (sizes[0], sizes[1]);
    assert!(
        lz4 * 2 < plain,
        "lz4 should be well under half of {plain} bytes on data this repetitive, got {lz4}"
    );
}

/// The default is size-triggered, so a large enough spool contains both codecs — and
/// every record has to come back regardless of which one it used.
#[test]
fn a_spool_may_mix_codecs() {
    // 32 MB is the switch point; go comfortably past it. Wide-ish rows so this needs
    // few batches: 2048 rows x 8 columns x 8 bytes is ~128 KB per batch.
    const COLS: usize = 8;
    const BATCHES: i64 = 400;
    let fields: Vec<Field> = (0..COLS)
        .map(|c| Field::new(format!("c{c}"), DataType::Int64, true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    // Incompressible, so the size trigger is not reached early by a lucky ratio.
    let scramble = |i: i64| -> i64 {
        let mut x = i as u64 ^ 0x9E37_79B9_7F4A_7C15;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        (x ^ (x >> 27)) as i64
    };

    std::env::remove_var("VGI_MR_SPOOL_COMPRESSION");
    let scope = format!("mixed-codec-{}", std::process::id()).into_bytes();
    let mut expected_rows = 0usize;
    for b in 0..BATCHES {
        let cols: Vec<ArrayRef> = (0..COLS)
            .map(|c| {
                Arc::new(Int64Array::from(
                    (0..2048i64)
                        .map(|r| scramble(b * 4096 + r + c as i64))
                        .collect::<Vec<_>>(),
                )) as ArrayRef
            })
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), cols).unwrap();
        expected_rows += batch.num_rows();
        assert!(spool::append(&scope, &batch, Some(b)).unwrap());
    }

    let back = spool::read_all(&scope).unwrap();
    assert_eq!(back.len(), BATCHES as usize, "every batch must read back");
    assert_eq!(
        back.iter().map(|(_, b)| b.num_rows()).sum::<usize>(),
        expected_rows
    );
    // The switch really happened: the spool is smaller than the plain encoding of the
    // same data would be, but not as small as compressing all of it, so both codecs are
    // present. (Plain is ~8 bytes per value plus framing.)
    let bytes = spool::sink_bytes(&scope);
    let plain_estimate = (expected_rows * COLS * 8) as u64;
    assert!(
        bytes < plain_estimate,
        "expected the tail to be compressed: {bytes} vs a plain estimate of {plain_estimate}"
    );
    assert!(
        bytes > 32 * 1024 * 1024,
        "expected the first 32 MB to be uncompressed, got only {bytes} bytes"
    );
    spool::discard(&scope);
}
