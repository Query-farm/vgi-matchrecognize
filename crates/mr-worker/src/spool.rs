//! A local append-only spool for buffered batches, bypassing `FunctionStorage`.
//!
//! Buffering the input is most of what a `match_recognize` query costs, and almost
//! all of *that* was one line: the SDK's SQLite `append` measured **93.5 ns/row**
//! against 4.3 ns/row to serialise the same data as Arrow IPC. Every 2048-row chunk
//! took a process-wide `Mutex<Connection>` and ran two statements (an `INSERT` plus a
//! `touch` UPSERT) — for a payload we never query, join, or index. We only ever write
//! it once and read it back once, in order.
//!
//! So when the execution is local, each sink thread appends to its own file instead:
//!
//! ```text
//! $TMPDIR/vgi-mr-<uid>/<hex(execution scope)>/<pid>-<n>.mrspool
//! record: batch_index: i64 LE | len: u32 LE | Arrow IPC stream bytes
//! ```
//!
//! # Why this is safe
//!
//! - **No buffering in the worker.** One `write_all` per `process()` call, straight to
//!   the OS. There is no end-of-input hook a buffering function can use (the SDK's
//!   `table_buffering_destructor` never calls into the function), so a userspace
//!   buffer would have no reliable point to flush at, and its tail would be lost.
//! - **Cross-process visibility.** Both durable SDK backends already rely on a shared
//!   `$TMPDIR` namespaced by real uid, which is what makes the sink and source phases
//!   able to run in different pooled worker processes. This uses the same root and the
//!   same 0700 hardening.
//! - **Failure is loud.** `append` on the SDK store is infallible by signature and
//!   reports loss through a negative id; a file write returns an error. A short or
//!   torn record is detectable too, since every record is length-prefixed.
//! - **One thread per file.** Files are keyed by pid and a per-thread number, so
//!   concurrent sinks never interleave into one file and no lock is needed.
//!
//! Reading is mode-agnostic: [`crate::buffer::read_batches`] reads the spool *and* the
//! SDK log and merges them, so a sink that fell back (wasm, an unwritable temp dir)
//! mixes with one that did not.
//!
//! # Cleanup
//!
//! A cancelled query gives the worker no hook at all — finalize may never run — so
//! orphans are inevitable and [`sweep`] handles them: on first use in a process, spool
//! directories older than `VGI_BUFFERING_STORE_TTL_SECS` (default 24h, the same knob
//! the SDK's own `gc` uses) are removed. The normal path deletes the execution's
//! directory when its producer is dropped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use arrow_array::RecordBatch;
use vgi::ipc;
use vgi_rpc::{Result, RpcError};

/// Fixed-width record header: the batch index, then the IPC payload length.
const HEADER: usize = 8 + 4;

/// Default age at which an orphaned spool directory is swept.
const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

fn re(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

thread_local! {
    /// Open spool files for this thread, keyed by execution scope. Thread-local so
    /// appending needs no lock, and so each file has exactly one writer.
    static FILES: RefCell<HashMap<Vec<u8>, File>> = RefCell::new(HashMap::new());
    /// This thread's number, for its file name.
    static THREAD_NO: u64 = NEXT_THREAD_NO.fetch_add(1, Ordering::Relaxed);
}

static NEXT_THREAD_NO: AtomicU64 = AtomicU64::new(0);

/// The root every execution's spool directory lives under, or `None` where a local
/// spool cannot work (wasm has no durable filesystem, and all phases share one
/// address space there anyway, so the in-process store is the right answer).
pub fn root() -> Option<PathBuf> {
    if cfg!(target_arch = "wasm32") {
        return None;
    }
    Some(std::env::temp_dir().join(format!("vgi-mr-{}", user_tag())))
}

/// A per-user directory name, so two users on a machine with a shared `/tmp` do not
/// contend for one root.
///
/// This is a *convenience*, not the security boundary — that is the 0700 mode on the
/// directory, plus the fact that failing to create or open a spool falls back to the
/// SDK store rather than proceeding. So an environment that lies about the user costs
/// at most a fallback, never someone else's data.
fn user_tag() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let tag: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if tag.is_empty() {
        "shared".to_string()
    } else {
        tag
    }
}

/// This execution's spool directory.
pub fn dir(scope: &[u8]) -> Option<PathBuf> {
    Some(root()?.join(hex(scope)))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Restrict a directory to its owner, as the SDK's stores do for the same reason:
/// buffered query data must not be world-readable in a shared temp dir.
#[cfg(unix)]
fn harden(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden(_path: &std::path::Path) {}

/// Append one batch to this thread's spool file.
///
/// `Ok(false)` means the spool is unavailable and the caller should use the SDK
/// store instead — a decision made per call, so a mixed run still works. `Err` means
/// the spool exists but the write failed, which must not be swallowed.
pub fn append(scope: &[u8], batch: &RecordBatch, batch_index: Option<i64>) -> Result<bool> {
    let Some(dir) = dir(scope) else {
        return Ok(false);
    };
    let payload = ipc::write_batch(batch)?;
    let mut record = Vec::with_capacity(HEADER + payload.len());
    // An absent index sorts before every real one, and the merge is stable, so a run
    // without indices keeps arrival order — the same convention the SDK log uses.
    record.extend_from_slice(&batch_index.unwrap_or(i64::MIN).to_le_bytes());
    let Ok(len) = u32::try_from(payload.len()) else {
        return Err(re("match_recognize: buffered batch exceeds 4 GiB"));
    };
    record.extend_from_slice(&len.to_le_bytes());
    record.extend_from_slice(&payload);

    FILES.with(|files| {
        let mut files = files.borrow_mut();
        if !files.contains_key(scope) {
            sweep_once();
            if fs::create_dir_all(&dir).is_err() {
                return Ok(false);
            }
            harden(&dir);
            let no = THREAD_NO.with(|n| *n);
            let path = dir.join(format!("{}-{}.mrspool", std::process::id(), no));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    files.insert(scope.to_vec(), f);
                }
                // Unwritable temp dir: fall back rather than failing the query.
                Err(_) => return Ok(false),
            }
        }
        let f = files.get_mut(scope).expect("just inserted");
        f.write_all(&record).map_err(|e| {
            re(format!(
                "match_recognize: writing the buffered batch to {} failed: {e}",
                dir.display()
            ))
        })?;
        Ok(true)
    })
}

/// Read every spooled batch of an execution, as `(batch_index, batch)` pairs.
///
/// An empty or absent directory is not an error: it just means nothing was spooled
/// (every sink fell back, or there was no input). Whether that is legitimate is
/// decided by the sink count in [`crate::buffer::FinalizeState`].
pub fn read_all(scope: &[u8]) -> Result<Vec<(i64, RecordBatch)>> {
    let Some(dir) = dir(scope) else {
        return Ok(Vec::new());
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    // Sorted by name so a run is reproducible when two files hold the same index
    // (only possible without batch indices, where arrival order is all there is).
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mrspool"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let bytes = fs::read(&path).map_err(|e| {
            re(format!(
                "match_recognize: reading {} failed: {e}",
                path.display()
            ))
        })?;
        let mut at = 0usize;
        while at < bytes.len() {
            if at + HEADER > bytes.len() {
                return Err(re(format!(
                    "match_recognize: truncated record header in {} at byte {at}",
                    path.display()
                )));
            }
            let index = i64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
            let len = u32::from_le_bytes(bytes[at + 8..at + HEADER].try_into().expect("4 bytes"))
                as usize;
            at += HEADER;
            let end = at
                .checked_add(len)
                .filter(|e| *e <= bytes.len())
                .ok_or_else(|| {
                    re(format!(
                        "match_recognize: truncated batch in {} (record claims {len} bytes)",
                        path.display()
                    ))
                })?;
            out.push((index, ipc::read_batch(&bytes[at..end])?));
            at = end;
        }
    }
    Ok(out)
}

/// Drop this thread's handle on an execution's spool, then delete its directory.
///
/// Best effort: an orphan left by a query that died before finalize is swept later.
pub fn discard(scope: &[u8]) {
    FILES.with(|files| {
        files.borrow_mut().remove(scope);
    });
    if let Some(dir) = dir(scope) {
        let _ = fs::remove_dir_all(dir);
    }
}

fn ttl() -> Duration {
    let secs = std::env::var("VGI_BUFFERING_STORE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS);
    Duration::from_secs(secs)
}

/// Sweep spool directories older than the TTL. Runs once per process, on first use.
fn sweep_once() {
    use std::sync::OnceLock;
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        sweep(ttl());
    });
}

/// Remove spool directories not modified within `ttl`.
///
/// Cancellation before finalize leaves the worker no hook, so this is the only thing
/// that bounds disk use over a long-lived pool. Mirrors the SDK's `gc`, including its
/// TTL knob, so operators have one thing to tune.
pub fn sweep(ttl: Duration) {
    let Some(root) = root() else { return };
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(ttl);
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Newest mtime in the directory: a spool still being written to is fresh.
        let newest = fs::read_dir(&path)
            .ok()
            .map(|files| {
                files
                    .flatten()
                    .filter_map(|f| f.metadata().ok()?.modified().ok())
                    .max()
            })
            .and_then(|m| m.or_else(|| entry.metadata().ok()?.modified().ok()));
        match (newest, cutoff) {
            (Some(m), Some(c)) if m < c => {
                let _ = fs::remove_dir_all(&path);
            }
            _ => {}
        }
    }
}
