//! The buffered-batch log: how `match_recognize` writes rows into cross-process
//! storage and reads them back.
//!
//! Both directions live here because they share one fragile convention — the scan
//! cursor. `scan` returns entries with `id > after_id`, and the storage contract
//! says only that an id is *monotonic*, not where it starts: SQLite's log is
//! `INTEGER PRIMARY KEY AUTOINCREMENT` so its first id is 1, while the filesystem
//! store computes `max_id + 1` over an empty directory so its first id is 0.
//! Paging from `after_id = 0` therefore skips the very first record on a 0-based
//! backend — which cost one whole batch (measured with the `fs` backend: 547
//! missing matches out of 1,333,333) and looked like backend data loss rather
//! than a cursor bug.

use arrow_array::RecordBatch;
use vgi::ipc;
use vgi::storage::SharedStorage;
use vgi_rpc::{Result, RpcError};

/// Where paging starts. Below every id either local backend assigns (1 for
/// SQLite, 0 for the filesystem store), so the first page includes everything.
/// Do not use `0`.
///
/// This is a floor, not a proof: because the filter is `id > after_id`, an entry
/// whose id was exactly `i64::MIN` would still be missed. No backend assigns that,
/// and one cannot be reached by incrementing from either origin above, but the
/// only way to remove the assumption entirely would be a `scan` that takes an
/// inclusive cursor.
const START_CURSOR: i64 = i64::MIN;

/// How many log entries to request per `scan` call.
const PAGE: usize = 256;

/// The batches themselves.
const NS_BATCHES: &[u8] = b"match_recognize";

/// One row-count record per buffered batch, written separately from the batch.
///
/// Checked against the batches at finalize: if the two totals disagree, the store
/// lost or duplicated something and we fail loudly instead of returning a short
/// answer. (This is a guard against *write*-side loss; the cursor bug above was
/// invisible to it precisely because it truncated both namespaces alike.)
const NS_ROWS: &[u8] = b"match_recognize.rows";

fn re(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// A finalize state id: the buffering scope, plus how many sink states fed it.
///
/// The count travels *outside* the store, so it stays trustworthy even when the
/// store does not. If sinks buffered batches and the finalize phase reads none
/// back, the two phases are not seeing the same state — which is what an
/// in-process storage backend looks like when the phases land in different worker
/// processes — and that must be an error, not an empty answer.
///
/// The SDK treats a finalize state id as opaque (`combine`'s return value is handed
/// straight to `finalize_producer`), so a fixed-width prefix is safe to add.
pub struct FinalizeState {
    pub scope: Vec<u8>,
    pub sink_count: u32,
}

impl FinalizeState {
    pub fn encode(scope: &[u8], sink_count: u32) -> Vec<u8> {
        let mut out = sink_count.to_le_bytes().to_vec();
        out.extend_from_slice(scope);
        out
    }

    pub fn decode(id: &[u8]) -> Result<Self> {
        if id.len() < 4 {
            return Err(re("match_recognize: malformed finalize state id"));
        }
        let (count, scope) = id.split_at(4);
        Ok(FinalizeState {
            scope: scope.to_vec(),
            sink_count: u32::from_le_bytes(count.try_into().expect("4 bytes")),
        })
    }
}

/// Read every entry of `ns` under `scope`, in id order.
///
/// Public so the storage-backend probe can exercise the worker's real paging
/// convention against each backend rather than a copy of it.
pub fn scan_log(storage: &SharedStorage, scope: &[u8], ns: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut after_id = START_CURSOR;
    loop {
        let page = storage.scan(scope, ns, b"", after_id, PAGE);
        if page.is_empty() {
            break;
        }
        for (id, bytes) in page {
            after_id = id;
            out.push(bytes);
        }
    }
    out
}

/// Buffer one (already projected) batch, plus its independent row-count record.
///
/// `batch_index` is DuckDB's per-chunk index, present because the function declares
/// `requires_input_batch_index`. It is written as a fixed-width header on the batch
/// record itself rather than into a side namespace: two logs would have to be paired
/// positionally, and a parallel sink can interleave appends so that the k-th batch
/// and the k-th side record come from different `process` calls. Self-describing
/// records need no pairing.
///
/// `append` is infallible by signature but reports failure through its return
/// value — the SQLite backend logs and returns `-1` without storing the row — so
/// both ids are checked. Dropping a batch here would otherwise surface as a
/// quietly short result rather than an error.
pub fn append_batch(
    storage: &SharedStorage,
    scope: &[u8],
    batch: &RecordBatch,
    batch_index: Option<i64>,
) -> Result<()> {
    // Prefer the local spool: the SDK store's append is ~20x the cost of serialising
    // the batch, for a payload that is only ever written once and read back once. It
    // declines (rather than failing) where a local file cannot work, and finalize
    // reads both sources, so a mixed run is fine.
    if crate::spool::append(scope, batch, batch_index)? {
        return Ok(());
    }
    // Absent index sorts before every real one, and the sort below is stable, so a
    // run without indices keeps arrival order — exactly the old behaviour.
    let mut record = batch_index.unwrap_or(i64::MIN).to_le_bytes().to_vec();
    record.extend_from_slice(&ipc::write_batch(batch)?);
    let batch_id = storage.append(scope, NS_BATCHES, b"", record);
    let count_id = storage.append(
        scope,
        NS_ROWS,
        b"",
        (batch.num_rows() as u64).to_le_bytes().to_vec(),
    );
    if batch_id < 0 || count_id < 0 {
        return Err(re(
            "match_recognize: the shared storage backend refused a buffered batch, so the result \
             would be silently incomplete. Check the worker log for the underlying storage error.",
        ));
    }
    Ok(())
}

/// Read back every buffered batch, verifying that the relation arrived intact.
///
/// Two independent checks, because a short read here would surface as a confidently
/// wrong answer rather than an error:
///
/// 1. `state.sink_count` sinks ran, so reading back *nothing* means the sink and
///    source phases are not sharing state. Carried outside the store, so it holds
///    even if the store is the thing that is broken.
/// 2. Every batch was written with a separate row-count record; the two totals must
///    agree, or the store dropped or duplicated something.
pub fn read_batches(storage: &SharedStorage, state: &FinalizeState) -> Result<Vec<RecordBatch>> {
    let scope = &state.scope;
    let mut indexed: Vec<(i64, RecordBatch)> = Vec::new();
    // The local spool first, then the SDK log. Which sink used which is not recorded
    // anywhere: reading both and merging on the batch index is simpler than a mode
    // flag, and it means a sink that fell back mixes with one that did not.
    let spooled = crate::spool::read_all(scope)?;
    let spooled_rows: usize = spooled.iter().map(|(_, b)| b.num_rows()).sum();
    indexed.extend(spooled);
    for bytes in scan_log(storage, scope, NS_BATCHES) {
        if bytes.len() < 8 {
            return Err(re("match_recognize: truncated buffered batch record"));
        }
        let (head, payload) = bytes.split_at(8);
        let index = i64::from_le_bytes(head.try_into().expect("8 bytes"));
        indexed.push((index, ipc::read_batch(payload)?));
    }
    // Restore the input order. Batches arrive in whatever order a parallel sink
    // produced them, so without this the buffered relation is in arrival order and
    // rows tying on the ORDER BY key would come out differently run to run.
    // A stable sort keeps arrival order among equal indices.
    indexed.sort_by_key(|(index, _)| *index);
    let batches: Vec<RecordBatch> = indexed.into_iter().map(|(_, b)| b).collect();
    let buffered_rows: usize = batches.iter().map(|b| b.num_rows()).sum();

    // Row-count records exist only for batches that went through the SDK store; the
    // spool needs none, because a file write reports its own failure and every record
    // is length-prefixed, so loss there cannot be silent.
    let mut expected_rows = spooled_rows;
    for bytes in scan_log(storage, scope, NS_ROWS) {
        let n: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| re("match_recognize: corrupt buffered row-count record"))?;
        expected_rows += u64::from_le_bytes(n) as usize;
    }

    if batches.is_empty() && state.sink_count > 0 {
        return Err(re(format!(
            "match_recognize: {} buffering sink(s) ran but the finalize phase read back no \
             buffered rows, so the two phases are not sharing state. This normally means the \
             shared storage backend cannot outlive a single worker process; unset \
             VGI_WORKER_SHARED_STORAGE to use the default durable store.",
            state.sink_count
        )));
    }
    if expected_rows != buffered_rows {
        return Err(re(format!(
            "match_recognize: read back {buffered_rows} buffered rows but {expected_rows} were \
             written — the shared storage backend lost data, so the result would be silently \
             incomplete. This is a bug; please report it with the storage backend in use."
        )));
    }
    Ok(batches)
}
