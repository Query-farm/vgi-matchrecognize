//! The buffered-batch log: how `match_recognize` writes rows into cross-process
//! storage and reads them back.
//!
//! Both directions live here because they share one fragile convention — the scan
//! cursor. The storage layer's `scan` returns entries with `id > after_id`, and the
//! ids are only documented as *monotonic*: the SQLite backend starts them at 1,
//! but the filesystem backend starts at 0. Paging from `after_id = 0` therefore
//! silently skips the very first record on any backend that is 0-based — which
//! cost one whole batch (measured: 547 missing matches out of 1,333,333) and
//! looked like backend data loss rather than a cursor bug. `START_CURSOR` is
//! below every representable id, so the first page includes everything.

use arrow_array::RecordBatch;
use vgi::ipc;
use vgi::storage::SharedStorage;
use vgi_rpc::{Result, RpcError};

/// Cursor value that precedes every id a backend can assign. Do not use `0`.
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
pub fn append_batch(storage: &SharedStorage, scope: &[u8], batch: &RecordBatch) -> Result<()> {
    storage.append(scope, NS_BATCHES, b"", ipc::write_batch(batch)?);
    storage.append(
        scope,
        NS_ROWS,
        b"",
        (batch.num_rows() as u64).to_le_bytes().to_vec(),
    );
    Ok(())
}

/// Read back every buffered batch, verifying the row-count tally.
pub fn read_batches(storage: &SharedStorage, scope: &[u8]) -> Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    let mut buffered_rows = 0usize;
    for bytes in scan_log(storage, scope, NS_BATCHES) {
        let b = ipc::read_batch(&bytes)?;
        buffered_rows += b.num_rows();
        batches.push(b);
    }

    let mut expected_rows = 0usize;
    for bytes in scan_log(storage, scope, NS_ROWS) {
        let n: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| re("match_recognize: corrupt buffered row-count record"))?;
        expected_rows += u64::from_le_bytes(n) as usize;
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
