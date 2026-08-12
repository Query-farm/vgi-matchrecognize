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
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use arrow_array::RecordBatch;
use arrow_buffer::{Buffer, MutableBuffer};
use arrow_ipc::reader::StreamDecoder;
use vgi::ipc;
use vgi_rpc::{Result, RpcError};

/// Fixed-width record header, **padded to 64 bytes**, followed by the payload, followed
/// by padding to the next multiple of 64.
///
/// ```text
/// 0..8    batch_index  i64  (i64::MIN when the source could not supply one)
/// 8..12   stored_len   u32  bytes of payload as written
/// 12..16  raw_len      u32  bytes of payload before compression
/// 16..17  codec        u8
/// 17..64  zero padding
/// ```
///
/// Two things pay for that padding, both of which the previous 13-byte header gave away:
///
/// - **Alignment.** Arrow writes each buffer inside a payload 64-byte aligned *relative
///   to the payload start*, so a payload beginning at an arbitrary file offset has every
///   buffer misaligned, and a reader must reallocate and copy all of it. Padding the
///   header to 64 and each record up to a multiple of 64 keeps every payload 64-aligned,
///   which is what lets [`read_files`] decode without copying the data.
/// - **The memory budget.** `raw_len` is the uncompressed size, which is what predicts
///   the producer's in-memory peak. `shard_count` divides *that* by the budget; using
///   bytes on disk silently loosened the bound by the compression ratio.
///
/// 64 bytes of header on a ~49 KB record is 0.13%, and the codec being per record means a
/// spool stays readable when the setting changes between queries.
const HEADER: usize = 64;

/// Alignment for record starts and payload starts alike; arrow's own buffer alignment.
const RECORD_ALIGN: usize = 64;

/// `n` rounded up to a multiple of [`RECORD_ALIGN`].
fn aligned(n: usize) -> usize {
    n.div_ceil(RECORD_ALIGN) * RECORD_ALIGN
}

/// Payload is a plain Arrow IPC stream.
const CODEC_NONE: u8 = 0;
/// Payload is an Arrow IPC stream, LZ4-compressed whole, with its uncompressed length
/// prepended by `lz4_flex`.
const CODEC_LZ4_FRAME: u8 = 1;

/// Most executions one thread keeps a spool file open for. See `FILES`.
const MAX_OPEN_SINK_FILES: usize = 32;

/// Default age at which an orphaned spool directory is swept.
const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

fn re(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// Bytes a sink writes uncompressed before it starts compressing.
///
/// Compression is worth CPU only where the bytes hurt, and for a small query they never
/// do: the whole spool stays in the page cache and is deleted seconds later. So a sink
/// starts plain and switches once it has written this much — which costs a short query
/// nothing and still compresses all but the first slice of a large one. Counted
/// *uncompressed*, so the threshold means the same thing on both sides of itself.
const COMPRESS_AFTER_BYTES: u64 = 32 * 1024 * 1024;

/// Whether to compress spooled batches: on past [`COMPRESS_AFTER_BYTES`], and
/// `VGI_MR_SPOOL_COMPRESSION=lz4|none` forces it either way.
///
/// This was briefly off by default, for a reason now fixed. The finalize phase decides
/// how many shards it needs by dividing the spool's size by a budget that means *the
/// producer's in-memory peak* ([`crate::shard::budget_bytes`]) — and it used to measure
/// bytes on disk, which stops predicting that as soon as anything is compressed, quietly
/// loosening the bound by the compression ratio. Each record now carries its uncompressed
/// length and [`sink_uncompressed_bytes`] totals *that*, so the two are comparable again
/// and compression is safe to enable.
///
/// The payload is compressed **whole** rather than through arrow's `IpcWriteOptions`.
/// Arrow compresses each buffer separately — per column, per batch — and the split's
/// ~205-row pieces made that catastrophic: reading shards back cost 346-507 ns/row
/// against 20 for whole-record framing.
///
/// Compression is applied to the **whole IPC payload as one blob**, not through Arrow's
/// `IpcWriteOptions`. Arrow's scheme compresses each buffer separately — one per column,
/// plus each validity bitmap — which is the wrong granularity here: the shard split
/// fragments a 2048-row batch into ~205-row slivers, so those buffers are around a
/// kilobyte and per-call overhead swamps the payload. Measured that way, reading shards
/// back cost 346-507 ns/row against 16 uncompressed. One call per record instead of one
/// per buffer also gives the codec a bigger window, so the ratio is better.
///
/// We can choose freely because the spool is ours end to end: written here, read back by
/// [`crate::buffer::read_batches`], never shipped anywhere.
fn compression(written: u64) -> u8 {
    match std::env::var("VGI_MR_SPOOL_COMPRESSION")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        // Forced on: from the first record, no threshold.
        "lz4" | "on" | "1" => CODEC_LZ4_FRAME,
        "none" | "off" | "0" => CODEC_NONE,
        // Unset, or a typo: decide by size rather than failing a query over an
        // environment variable.
        _ if written >= COMPRESS_AFTER_BYTES => CODEC_LZ4_FRAME,
        _ => CODEC_NONE,
    }
}

/// One encoded record's payload: how it is encoded, its uncompressed size, and the bytes
/// as they will be stored.
struct Encoded {
    codec: u8,
    raw_len: u32,
    payload: Vec<u8>,
}

/// Serialise one batch, and say how.
fn encode_batch(batch: &RecordBatch, written: u64) -> Result<Encoded> {
    let ipc_bytes = ipc::write_batch(batch)?;
    let raw_len = u32::try_from(ipc_bytes.len())
        .map_err(|_| re("match_recognize: buffered batch exceeds 4 GiB"))?;
    match compression(written) {
        CODEC_LZ4_FRAME => Ok(Encoded {
            codec: CODEC_LZ4_FRAME,
            raw_len,
            payload: lz4_flex::block::compress_prepend_size(&ipc_bytes),
        }),
        _ => Ok(Encoded {
            codec: CODEC_NONE,
            raw_len,
            payload: ipc_bytes,
        }),
    }
}

/// Lay out one record: padded header, payload, padding to the next 64-byte boundary.
fn frame_record(batch_index: i64, enc: &Encoded) -> Result<Vec<u8>> {
    let stored_len = u32::try_from(enc.payload.len())
        .map_err(|_| re("match_recognize: buffered batch exceeds 4 GiB"))?;
    let total = aligned(HEADER + enc.payload.len());
    let mut record = vec![0u8; total];
    record[0..8].copy_from_slice(&batch_index.to_le_bytes());
    record[8..12].copy_from_slice(&stored_len.to_le_bytes());
    record[12..16].copy_from_slice(&enc.raw_len.to_le_bytes());
    record[16] = enc.codec;
    record[HEADER..HEADER + enc.payload.len()].copy_from_slice(&enc.payload);
    Ok(record)
}

/// Decode one record's payload into a batch.
///
/// Goes through arrow's own [`StreamDecoder`] over an aligned [`Buffer`], not the SDK's
/// `ipc::read_batch`. Two reasons: the SDK reader copies the whole payload through a
/// `Read` adapter and rewrites the schema's nullability on every record — pointless here,
/// since we wrote that schema ourselves from a real batch — and it cannot hand out arrays
/// that borrow the input. Decoding from a `Buffer` whose payload is 64-byte aligned lets
/// the resulting arrays point *into* it, so an uncompressed record costs no copy of its
/// data at all.
fn decode_payload(codec: u8, raw_len: u32, payload: Buffer) -> Result<RecordBatch> {
    let mut buffer = match codec {
        CODEC_NONE => payload,
        CODEC_LZ4_FRAME => {
            // Decompress into an arrow buffer rather than a `Vec`: `MutableBuffer` is
            // 64-byte aligned, so the arrays can borrow from it just as in the
            // uncompressed case. The first four bytes are lz4_flex's size prefix.
            let mut out = MutableBuffer::from_len_zeroed(raw_len as usize);
            let written =
                lz4_flex::block::decompress_into(&payload.as_slice()[4..], out.as_slice_mut())
                    .map_err(|e| {
                        re(format!(
                            "match_recognize: corrupt compressed spool record: {e}"
                        ))
                    })?;
            if written != raw_len as usize {
                return Err(re(format!(
                    "match_recognize: spool record decompressed to {written} bytes, header says \
                     {raw_len}"
                )));
            }
            out.into()
        }
        other => {
            return Err(re(format!(
                "match_recognize: spool record has unknown codec {other}; the file was written \
                 by a different version of this worker"
            )))
        }
    };
    let mut decoder = StreamDecoder::new();
    match decoder.decode(&mut buffer) {
        Ok(Some(batch)) => Ok(batch),
        Ok(None) => Err(re("match_recognize: spool record holds no batch")),
        Err(e) => Err(re(format!("match_recognize: decoding a spool record: {e}"))),
    }
}

/// One thread's open spool file for one execution, and how much it has written.
struct SinkFile {
    file: File,
    written: u64,
}

thread_local! {
    /// Open spool files for this thread, keyed by execution scope. Thread-local so
    /// appending needs no lock, and so each file has exactly one writer.
    ///
    /// Bounded, because nothing here learns that an execution ended: `discard` runs in
    /// whichever process owns the *producer*, and with a pooled transport that need not
    /// be the process that sank the rows. Without a cap, a long-lived worker would hold
    /// a file descriptor and a scope key for every execution it had ever served.
    /// Dropping an entry is always safe — the next append reopens in append mode and
    /// continues where it left off.
    static FILES: RefCell<HashMap<Vec<u8>, SinkFile>> = RefCell::new(HashMap::new());
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
    FILES.with(|files| {
        let mut files = files.borrow_mut();
        if !files.contains_key(scope) {
            if files.len() >= MAX_OPEN_SINK_FILES {
                files.clear();
            }
            sweep_once();
            if fs::create_dir_all(&dir).is_err() {
                return Ok(false);
            }
            harden(&dir);
            let no = THREAD_NO.with(|n| *n);
            let path = dir.join(format!("sink-{}-{}.mrspool", std::process::id(), no));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    files.insert(scope.to_vec(), SinkFile { file, written: 0 });
                }
                // Unwritable temp dir: fall back rather than failing the query.
                Err(_) => return Ok(false),
            }
        }
        let f = files.get_mut(scope).expect("just inserted");
        // How much this sink has written decides whether the record is compressed; see
        // `COMPRESS_AFTER_BYTES`. Counted uncompressed, so the threshold means what it
        // says whichever side of it the sink is on.
        let enc = encode_batch(batch, f.written)?;
        // An absent index sorts before every real one, and the merge is stable, so a run
        // without indices keeps arrival order — the same convention the SDK log uses.
        let record = frame_record(batch_index.unwrap_or(i64::MIN), &enc)?;
        f.written += enc.raw_len as u64;
        f.file.write_all(&record).map_err(|e| {
            re(format!(
                "match_recognize: writing the buffered batch to {} failed: {e}",
                dir.display()
            ))
        })?;
        Ok(true)
    })
}

/// The spool files matching a prefix, sorted by name.
///
/// Sorted so a run is reproducible when two files hold the same batch index (only
/// possible without indices, where arrival order is all there is).
pub fn files_with_prefix(scope: &[u8], prefix: &str) -> Vec<PathBuf> {
    let Some(dir) = dir(scope) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "mrspool")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    paths.sort();
    paths
}

/// How many *uncompressed* bytes an execution has spooled from its sinks.
///
/// This is the figure the shard count is derived from, and it has to be the uncompressed
/// one: `shard_count` divides it by a budget that means the producer's in-memory peak, so
/// measuring bytes on disk instead loosened that bound by whatever the compression ratio
/// happened to be.
///
/// Reads only headers — 64 bytes per record, seeking over each payload — so this costs
/// seeks rather than a pass over the data.
pub fn sink_uncompressed_bytes(scope: &[u8]) -> u64 {
    let mut total = 0u64;
    for path in files_with_prefix(scope, "sink-") {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let mut header = [0u8; HEADER];
        while reader.read_exact(&mut header).is_ok() {
            let stored_len =
                u32::from_le_bytes(header[8..12].try_into().expect("4 bytes")) as usize;
            total += u64::from(u32::from_le_bytes(
                header[12..16].try_into().expect("4 bytes"),
            ));
            let skip = (aligned(HEADER + stored_len) - HEADER) as i64;
            if reader.seek_relative(skip).is_err() {
                break;
            }
        }
    }
    total
}

/// The open shard files of one split pass.
///
/// The split writes a piece of every input batch to every shard it touches, so opening
/// the file per piece means `batches x shards` open/write/close cycles — measured at
/// 48,830 of them for a 10M-row query, and **86% of the split's entire cost** (259
/// ns/row, of which deciding, gathering and encoding the rows accounted for 35).
/// Holding the files open for the pass removes that.
///
/// Buffered, which the sink path deliberately is *not*: `split` is one call with a
/// defined end, so [`ShardWriters::finish`] is a guaranteed flush point. `process()`
/// has no such point, which is why it writes straight through.
pub struct ShardWriters {
    dir: PathBuf,
    /// One per shard, created on first use — a shard that receives no rows gets no
    /// file, which `read_shard` already treats as an empty shard.
    writers: Vec<Option<BufWriter<File>>>,
    buffer_capacity: usize,
}

/// Total buffering across all shards, split evenly. Bounded so that a query sharded
/// into hundreds of pieces does not turn its write buffers into a memory problem of
/// their own.
const SHARD_BUFFER_TOTAL: usize = 8 * 1024 * 1024;

impl ShardWriters {
    pub fn new(scope: &[u8], shards: usize) -> Result<Self> {
        let Some(dir) = dir(scope) else {
            return Err(re("match_recognize: no spool directory to shard into"));
        };
        Ok(ShardWriters {
            dir,
            writers: (0..shards).map(|_| None).collect(),
            buffer_capacity: (SHARD_BUFFER_TOTAL / shards.max(1)).clamp(16 * 1024, 1024 * 1024),
        })
    }

    /// Append one batch to a shard, in the same framing the sink uses.
    pub fn append(&mut self, shard: usize, batch: &RecordBatch, batch_index: i64) -> Result<()> {
        // A split only happens for an input past the memory budget, which is far past the
        // size where compression is worth it — so shard records compress from the first
        // one (unless the environment says otherwise).
        let enc = encode_batch(batch, u64::MAX)?;
        let capacity = self.buffer_capacity;
        let dir = self.dir.clone();
        let slot = self
            .writers
            .get_mut(shard)
            .ok_or_else(|| re(format!("match_recognize: shard {shard} out of range")))?;
        if slot.is_none() {
            let path = dir.join(format!("shard{shard}.mrspool"));
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| {
                    re(format!(
                        "match_recognize: opening {} failed: {e}",
                        path.display()
                    ))
                })?;
            *slot = Some(BufWriter::with_capacity(capacity, f));
        }
        let w = slot.as_mut().expect("just created");
        let record = frame_record(batch_index, &enc)?;
        w.write_all(&record)
            .map_err(|e| re(format!("match_recognize: writing a shard failed: {e}")))
    }

    /// Flush every shard, propagating any failure.
    ///
    /// Explicit rather than left to `Drop`, which swallows errors: the finalize states
    /// are handed out immediately after this returns and may be read by another
    /// process, so a write that did not land has to be an error here.
    pub fn finish(mut self) -> Result<()> {
        for (shard, slot) in self.writers.iter_mut().enumerate() {
            if let Some(w) = slot {
                w.flush().map_err(|e| {
                    re(format!(
                        "match_recognize: flushing shard {shard} failed: {e}"
                    ))
                })?;
            }
        }
        Ok(())
    }
}

/// Delete one spool file.
///
/// Used by the shard split as each sink file is consumed, so the relation is never on
/// disk twice.
pub fn remove_file(path: &std::path::Path) {
    let _ = fs::remove_file(path);
}

/// Read one shard's batches.
pub fn read_shard(scope: &[u8], shard: usize) -> Result<Vec<(i64, RecordBatch)>> {
    read_files(&files_with_prefix(scope, &format!("shard{shard}.")))
}

/// Read every spooled batch of an execution, as `(batch_index, batch)` pairs.
///
/// An empty or absent directory is not an error: it just means nothing was spooled
/// (every sink fell back, or there was no input). Whether that is legitimate is
/// decided by the sink count in [`crate::buffer::FinalizeState`].
pub fn read_all(scope: &[u8]) -> Result<Vec<(i64, RecordBatch)>> {
    read_files(&files_with_prefix(scope, "sink-"))
}

/// Read a list of spool files into `(batch_index, batch)` pairs.
///
/// Streams: one record's payload is resident at a time, and the file's bytes are never
/// all in memory at once. The previous version read the whole file with `fs::read` and
/// then kept every decoded batch, so a producer's peak was the file *plus* its decoded
/// form — about twice its shard, against a memory budget that assumes once.
///
/// Each payload is read into a 64-byte-aligned `MutableBuffer`, which is what allows
/// `decode_payload` below to hand out arrays that borrow it rather than copying.
pub fn read_files(paths: &[PathBuf]) -> Result<Vec<(i64, RecordBatch)>> {
    let mut out = Vec::new();
    for path in paths {
        let file = File::open(path).map_err(|e| {
            re(format!(
                "match_recognize: opening {} failed: {e}",
                path.display()
            ))
        })?;
        let mut reader = BufReader::new(file);
        while let Some((index, batch)) = read_record(&mut reader)
            .map_err(|e| re(format!("match_recognize: reading {}: {e}", path.display())))?
        {
            out.push((index, batch));
        }
    }
    Ok(out)
}

/// Read one record, or `None` at a clean end of file.
fn read_record<R: Read>(reader: &mut R) -> Result<Option<(i64, RecordBatch)>> {
    let mut header = [0u8; HEADER];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(re(format!("truncated record header: {e}"))),
    }
    let index = i64::from_le_bytes(header[0..8].try_into().expect("8 bytes"));
    let stored_len = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes")) as usize;
    let raw_len = u32::from_le_bytes(header[12..16].try_into().expect("4 bytes"));
    let codec = header[16];

    // Aligned, so the decoded arrays can borrow it.
    let mut payload = MutableBuffer::from_len_zeroed(stored_len);
    reader.read_exact(payload.as_slice_mut()).map_err(|e| {
        re(format!(
            "truncated record payload of {stored_len} bytes: {e}"
        ))
    })?;
    // Records are padded to a multiple of 64; skip the tail so the next header is found.
    let padding = aligned(HEADER + stored_len) - HEADER - stored_len;
    if padding > 0 {
        let mut skip = [0u8; RECORD_ALIGN];
        reader
            .read_exact(&mut skip[..padding])
            .map_err(|e| re(format!("truncated record padding: {e}")))?;
    }
    Ok(Some((
        index,
        decode_payload(codec, raw_len, payload.into())?,
    )))
}

/// Delete one shard's file, and the execution's directory if that was the last.
///
/// A sharded execution has several producers reading the same directory, so no single
/// one may delete it — the first to finish would take the others' data with it. Each
/// removes only its own shard and then tries the directory, which succeeds for whoever
/// is last because `remove_dir` refuses a non-empty one.
pub fn discard_shard(scope: &[u8], shard: usize) {
    let Some(dir) = dir(scope) else { return };
    let _ = fs::remove_file(dir.join(format!("shard{shard}.mrspool")));
    let _ = fs::remove_dir(dir);
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
