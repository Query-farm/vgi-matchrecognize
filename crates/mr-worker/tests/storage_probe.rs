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
