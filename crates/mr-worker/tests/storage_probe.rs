//! Does the shared-storage log round-trip every appended record?
//!
//! The buffering function's correctness rests entirely on this: `process` appends
//! batches, `finalize_producer` scans them back, and anything lost in between is a
//! silently short answer. This probes the backends directly, independently of the
//! matcher, so a failure here points at storage rather than at the engine.

use std::sync::Arc;

use mr_worker::buffer::scan_log;
use vgi::storage::SharedStorage;

const NS: &[u8] = b"probe";
/// Payload size in the same ballpark as a buffered Arrow batch, so any
/// size-dependent behaviour (partial writes) has a chance to show up.
const PAYLOAD: usize = 4096;

fn payload(i: u64) -> Vec<u8> {
    let mut v = vec![0u8; PAYLOAD];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

/// Read every record back through the worker's own paging helper, so this test
/// guards the real convention rather than a copy of it.
fn scan_all(store: &SharedStorage, scope: &[u8]) -> Vec<u64> {
    scan_log(store, scope, NS)
        .into_iter()
        .map(|bytes| {
            if bytes.len() != PAYLOAD {
                // A torn record: present but not what was written.
                u64::MAX
            } else {
                u64::from_le_bytes(bytes[..8].try_into().unwrap())
            }
        })
        .collect()
}

fn check(store: &SharedStorage, scope: &[u8], expect: usize, label: &str) {
    let seen = scan_all(store, scope);
    let torn = seen.iter().filter(|v| **v == u64::MAX).count();
    let mut ids: Vec<u64> = seen.iter().copied().filter(|v| *v != u64::MAX).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(torn, 0, "{label}: {torn} torn records");
    assert_eq!(
        seen.len(),
        expect,
        "{label}: read {} of {expect} appended records",
        seen.len()
    );
    assert_eq!(ids.len(), expect, "{label}: duplicate or missing payloads");
    store.clear(scope);
}

/// Sequential appends, paged back — exercises the paging loop across page borders.
#[test]
fn sqlite_log_round_trips_sequential() {
    let path = std::env::temp_dir().join(format!("mr-probe-{}.db", std::process::id()));
    let store: SharedStorage = Arc::new(vgi::storage::SqliteStorage::open(path.clone()));
    for n in [1usize, 255, 256, 257, 1000] {
        let scope = format!("sq-seq-{n}").into_bytes();
        for i in 0..n {
            store.append(&scope, NS, b"", payload(i as u64));
        }
        check(&store, &scope, n, &format!("sqlite sequential n={n}"));
    }
    let _ = std::fs::remove_file(&path);
}

/// Concurrent appends from several threads, as pooled workers would do.
#[test]
fn sqlite_log_round_trips_concurrent() {
    let path = std::env::temp_dir().join(format!("mr-probe-conc-{}.db", std::process::id()));
    let store: SharedStorage = Arc::new(vgi::storage::SqliteStorage::open(path.clone()));
    const THREADS: usize = 8;
    const PER: usize = 200;
    let scope: Vec<u8> = b"sq-conc".to_vec();
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let store: SharedStorage = Arc::clone(&store);
        let scope = scope.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER {
                store.append(&scope, NS, b"", payload((t * PER + i) as u64));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    check(&store, &scope, THREADS * PER, "sqlite concurrent");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fs_log_round_trips_sequential() {
    let store: SharedStorage = Arc::new(vgi::storage::FsStorage::new());
    for n in [1usize, 255, 256, 257, 1000] {
        let scope = format!("mr-probe-fs-seq-{}-{n}", std::process::id()).into_bytes();
        for i in 0..n {
            store.append(&scope, NS, b"", payload(i as u64));
        }
        check(&store, &scope, n, &format!("fs sequential n={n}"));
    }
}

#[test]
fn fs_log_round_trips_concurrent() {
    let store: SharedStorage = Arc::new(vgi::storage::FsStorage::new());
    const THREADS: usize = 8;
    const PER: usize = 200;
    let scope: Vec<u8> = format!("mr-probe-fs-conc-{}", std::process::id()).into_bytes();
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let store: SharedStorage = Arc::clone(&store);
        let scope = scope.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER {
                store.append(&scope, NS, b"", payload((t * PER + i) as u64));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    check(&store, &scope, THREADS * PER, "fs concurrent");
}

/// The out-of-band sink count must turn "the phases cannot see each other's state"
/// into an error rather than an empty result — the failure mode that motivated
/// carrying the count outside the store in the first place. This was the one guard
/// in `buffer.rs` with no test.
#[test]
fn finalize_errors_when_sinks_ran_but_nothing_was_buffered() {
    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use mr_worker::buffer::{append_batch, read_batches, FinalizeState};

    let path = std::env::temp_dir().join(format!("mr-probe-guard-{}.db", std::process::id()));
    let store: SharedStorage = Arc::new(vgi::storage::SqliteStorage::open(path.clone()));

    // Three sinks reported buffering, but the store hands back nothing.
    let scope = format!("guard-empty-{}", std::process::id()).into_bytes();
    let err = read_batches(
        &store,
        &FinalizeState {
            scope: scope.clone(),
            sink_count: 3,
            shard: 0,
            shards: 1,
            expect_rows: None,
        },
    )
    .expect_err("sinks ran but no rows read back must be an error");
    let msg = err.to_string();
    assert!(msg.contains("not sharing state"), "{msg}");
    assert!(
        msg.contains('3'),
        "the message should name the sink count: {msg}"
    );

    // No sinks and no rows is a legitimately empty relation, not a failure.
    read_batches(
        &store,
        &FinalizeState {
            scope: scope.clone(),
            sink_count: 0,
            shard: 0,
            shards: 1,
            expect_rows: None,
        },
    )
    .expect("an empty input must not error");

    // And the happy path still agrees on the row count, which is the other guard.
    let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(Int64Array::from(vec![1i64, 2, 3])) as arrow_array::ArrayRef],
    )
    .unwrap();
    append_batch(&store, &scope, &batch, Some(0)).unwrap();
    let back = read_batches(
        &store,
        &FinalizeState {
            scope: scope.clone(),
            sink_count: 1,
            shard: 0,
            shards: 1,
            expect_rows: None,
        },
    )
    .expect("a buffered batch must read back");
    assert_eq!(back.iter().map(|b| b.num_rows()).sum::<usize>(), 3);

    store.clear(&scope);
    // `append_batch` prefers the local spool, and only a producer's Drop deletes it.
    mr_worker::spool::discard(&scope);
    let _ = std::fs::remove_file(&path);
}

/// The local spool round-trips what it is given, including from several threads at
/// once — each writes its own file, so there is no interleaving and no lock.
#[test]
fn spool_round_trips_including_concurrent_writers() {
    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use mr_worker::spool;

    let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
    let batch = |from: i64, n: i64| {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((from..from + n).collect::<Vec<_>>()))
                    as arrow_array::ArrayRef,
            ],
        )
        .unwrap()
    };

    // Sequential: batch indices are preserved, not positions.
    let scope = format!("spool-seq-{}", std::process::id()).into_bytes();
    for i in 0..5i64 {
        assert!(
            spool::append(&scope, &batch(i * 10, 3), Some(i)).unwrap(),
            "the spool should be available on this platform"
        );
    }
    let mut back = spool::read_all(&scope).unwrap();
    back.sort_by_key(|(i, _)| *i);
    assert_eq!(back.len(), 5);
    assert_eq!(
        back.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert_eq!(back.iter().map(|(_, b)| b.num_rows()).sum::<usize>(), 15);
    spool::discard(&scope);
    assert!(
        spool::read_all(&scope).unwrap().is_empty(),
        "discard removes it"
    );

    // Concurrent: 8 threads x 50 batches, each thread on its own file.
    let scope = format!("spool-conc-{}", std::process::id()).into_bytes();
    const THREADS: i64 = 8;
    const PER: i64 = 50;
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let scope = scope.clone();
            let schema = schema.clone();
            s.spawn(move || {
                for k in 0..PER {
                    let b = RecordBatch::try_new(
                        schema.clone(),
                        vec![Arc::new(Int64Array::from(vec![t, k])) as arrow_array::ArrayRef],
                    )
                    .unwrap();
                    spool::append(&scope, &b, Some(t * PER + k)).unwrap();
                }
            });
        }
    });
    let back = spool::read_all(&scope).unwrap();
    assert_eq!(back.len(), (THREADS * PER) as usize, "no records lost");
    let mut seen: Vec<i64> = back.iter().map(|(i, _)| *i).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        (THREADS * PER) as usize,
        "no torn or duplicated records"
    );
    assert_eq!(
        back.iter().map(|(_, b)| b.num_rows()).sum::<usize>(),
        (THREADS * PER * 2) as usize
    );
    spool::discard(&scope);
}
