//! Splitting a buffered relation by partition key, so memory tracks a shard rather
//! than the whole input.
//!
//! Matching is per-partition, so the finalize phase needs a whole partition in memory
//! at once — but not the whole *relation*. Until now it read everything back anyway,
//! which made input size bound by RAM at roughly 120 bytes per row.
//!
//! When the spooled input is larger than a memory budget, `combine` rewrites it into
//! `S` shard files, sending every row of a partition to the same shard, and returns one
//! finalize state id per shard. Each producer then reads only its own shard, so peak
//! memory is about `total / S` instead of `total`.
//!
//! # It costs time to save memory
//!
//! Measured on an 8M-row query (2000 partitions), sharding at a 32 MB budget: peak
//! worker RSS 365 MB -> 167 MB, wall clock **1.17 s -> 2.20 s**. The rewrite is a
//! second full pass — decode, hash, `take`, re-encode, write, and then the producers
//! decode again — and it is serial, so DuckDB sits idle through it (its CPU time drops
//! while wall clock rises).
//!
//! So this is not a speedup and must not be sold as one. It is what makes a query whose
//! input does not fit in memory *finish at all*, which is why the budget defaults high
//! enough that ordinary queries never take this path. Hashing was measured and is not
//! the bottleneck (a typed key reader replacing the per-row `Value` changed nothing
//! outside noise), so the cost is the pass itself.
//!
//! Extra finalize ids do also give DuckDB something to drain in parallel, but that was
//! not observed to pay here: drain threads are sized from the *sink* worker count, and
//! matching is already parallel inside one producer.
//!
//! The split is skipped when it cannot help or is not needed:
//!
//! - Input within the budget: one shard, no rewrite, no extra pass. This is the common
//!   case and it costs nothing.
//! - No `partition_by`: the relation is a single partition by definition, so no
//!   assignment could divide it. Memory then tracks the input, and that is inherent —
//!   a match may span the whole partition.
//! - Nothing spooled (every sink fell back to the SDK store, or there was no input):
//!   the single-stream path handles it.
//!
//! Assignment happens in one process (`combine` is a single RPC) and only that process
//! decides which rows go where, so the hash needs no cross-process stability — it just
//! has to be consistent within the pass.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use mr_core::plan::Plan;
use mr_core::value::Value;
use vgi_rpc::{Result, RpcError};

use crate::arrow_in::BatchRowStore;

/// Spooled bytes above which the finalize phase shards. Roughly the peak the
/// producer will hold, since the IPC form and the in-memory form are close in size.
const DEFAULT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Ceiling on shards. Each one is a file and a finalize stream, so this bounds how many
/// of both a single query can create.
///
/// It has to be high enough that the memory budget stays a *bound* rather than a
/// suggestion: with 64 the budget stopped binding above 64 x 256 MB = 16 GB of buffered
/// input, and peak memory went back to growing linearly with the relation — which is
/// precisely what sharding exists to prevent. At 1024 the budget holds to ~256 GB of
/// input at the default, and further with a smaller one.
///
/// The cost of a high ceiling is file descriptors and finalize streams, and neither is
/// paid unless the input actually needs the shards: the count comes from
/// `input / budget`, so a query only reaches 1024 shards if it really is that large
/// relative to what it was told it may hold.
const MAX_SHARDS: usize = 1024;

fn re(e: impl std::fmt::Display) -> RpcError {
    RpcError::runtime_error(e.to_string())
}

/// The finalize memory budget, in bytes of spooled input.
pub fn budget_bytes() -> u64 {
    std::env::var("VGI_MR_FINALIZE_MEMORY_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_BUDGET_BYTES)
}

/// How many shards an execution of `total_bytes` needs.
///
/// `partitioned` is false when the query has no `partition_by`, in which case the
/// whole relation is one partition and no split can divide it.
pub fn shard_count(total_bytes: u64, partitioned: bool) -> usize {
    if !partitioned || total_bytes == 0 {
        return 1;
    }
    let budget = budget_bytes();
    let needed = total_bytes.div_ceil(budget) as usize;
    needed.clamp(1, MAX_SHARDS)
}

/// Target size of one shard record, in bytes of arrow data.
///
/// The split used to write each input batch's slice straight out, so with `s` shards its
/// records held `2048 / s` rows: at 10 shards ~205, at 1024 shards **two**. Every record
/// carries a fixed ~472 bytes of IPC framing (its own schema message), so that is 10%
/// overhead at 205 rows and **10.8x** at two — the shard files would come out an order of
/// magnitude larger than the input they came from. Accumulating rows per shard and writing
/// full-size records is what makes the high end of `MAX_SHARDS` usable at all.
///
/// 256 KB is where the returns stop: framing overhead is under 1% by ~2048 rows, LZ4's
/// window is 64 KB so a bigger record buys no further ratio, and 256 KB of columns still
/// sits in L2 while being processed.
const TARGET_RECORD_BYTES: usize = 256 * 1024;

/// Ceiling on rows accumulated across *all* shards at once.
///
/// Expressed in bytes, since the row width varies: this is what the pass may hold on top
/// of the one batch it is splitting, and it must stay small against the memory budget the
/// pass exists to enforce. With few shards each gets the full [`TARGET_RECORD_BYTES`];
/// with many, each gets a proportionally smaller slice rather than the total growing.
const ACCUM_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Rows accumulated for one shard, waiting to be written as one record.
struct Accum {
    batches: Vec<RecordBatch>,
    bytes: usize,
    /// Batch index of the first input record contributing to this group. Because the merge
    /// below consumes records in ascending index order, a group covers a contiguous range,
    /// so tagging it with the first index keeps the read-side sort exact.
    first_index: Option<i64>,
}

/// Rewrite an execution's spooled batches into `shards` shard files, keyed by partition
/// key, and delete the originals.
///
/// **Merges the sink files in global batch-index order** rather than walking them one at a
/// time. That matters for more than tidiness: sink files carry *strided* indices (one
/// thread wrote 0, 8, 16…, another 1, 9, 17…), so consuming them file by file and then
/// coalescing would tag a group with an index that no longer orders it against groups from
/// another file — the read-side sort would put every row of one sink before every row of
/// the next, instead of interleaving them. Rows tying on the `order_by` key would then come
/// out in an order that depends on how DuckDB happened to schedule its sinks, which is
/// exactly what the batch index exists to prevent (`test/sql/batch_index.test`).
///
/// Consuming in index order also means each coalesced group is index-contiguous, which is
/// what makes coalescing sound at all.
///
/// Peak disk stays at about the size of the relation: a sink file is deleted as soon as its
/// last record is consumed, so the shards grow as the sinks shrink.
pub fn split(
    scope: &[u8],
    plan: &Plan,
    projected_schema: &SchemaRef,
    shards: usize,
) -> Result<Vec<u64>> {
    debug_assert!(shards > 1, "splitting into one shard is a no-op");
    let mut rows_per_shard = vec![0u64; shards];
    let part_cols = plan.partition_by_columns();
    let mut writers = crate::spool::ShardWriters::new(scope, shards)?;
    let mut cursors = crate::spool::sink_cursors(scope)?;
    let per_shard_target = (ACCUM_BUDGET_BYTES / shards.max(1)).min(TARGET_RECORD_BYTES);
    let mut accums: Vec<Accum> = (0..shards)
        .map(|_| Accum {
            batches: Vec::new(),
            bytes: 0,
            first_index: None,
        })
        .collect();

    // The next record across all sinks, by batch index. Linear over the cursors, of which
    // there are as many as sink threads — a heap would be pointless at that size.
    let next_cursor = |cursors: &[crate::spool::RecordCursor]| -> Option<usize> {
        cursors
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.peek_index().map(|ix| (ix, i)))
            .min()
            .map(|(_, i)| i)
    };
    while let Some(pick) = next_cursor(&cursors) {
        let Some((index, batch)) = cursors[pick].next_batch()? else {
            continue;
        };
        if cursors[pick].peek_index().is_none() {
            // Exhausted: its rows are all in shards or in an accumulator, so the copy on
            // disk is dead weight. Dropping it here is what keeps peak disk at ~1x.
            crate::spool::remove_file(cursors[pick].path());
        }

        let store = BatchRowStore::new(projected_schema.clone(), vec![batch.clone()]);
        let cols: Vec<usize> = part_cols
            .iter()
            .map(|c| {
                mr_core::engine::RowStore::col_index(&store, c)
                    .ok_or_else(|| re(format!("match_recognize: unknown partition column '{c}'")))
            })
            .collect::<Result<_>>()?;

        // Row indices per shard, then one `take` per shard: a single pass over the batch
        // and one copy of each row.
        let mut per_shard: Vec<Vec<u32>> = vec![Vec::new(); shards];
        for row in 0..batch.num_rows() {
            let mut h = DefaultHasher::new();
            for &ci in &cols {
                hash_value(&mr_core::engine::RowStore::cell(&store, row, ci), &mut h);
            }
            let s = (h.finish() % shards as u64) as usize;
            per_shard[s].push(row as u32);
        }
        for (s, rows) in per_shard.iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let sub = take_rows(&batch, rows)?;
            rows_per_shard[s] += sub.num_rows() as u64;
            let accum = &mut accums[s];
            accum.first_index.get_or_insert(index);
            accum.bytes += sub.get_array_memory_size();
            accum.batches.push(sub);
            if accum.bytes >= per_shard_target {
                flush(&mut writers, s, accum, projected_schema)?;
            }
        }
    }

    for (s, accum) in accums.iter_mut().enumerate() {
        flush(&mut writers, s, accum, projected_schema)?;
    }
    // Before the row counts are handed out as finalize state, every byte must be on its way
    // to disk — another process may read these files.
    writers.finish()?;
    Ok(rows_per_shard)
}

/// Write one shard's accumulated rows as a single record, and reset the accumulator.
fn flush(
    writers: &mut crate::spool::ShardWriters,
    shard: usize,
    accum: &mut Accum,
    schema: &SchemaRef,
) -> Result<()> {
    if accum.batches.is_empty() {
        return Ok(());
    }
    let index = accum
        .first_index
        .take()
        .expect("non-empty group has an index");
    let merged = if accum.batches.len() == 1 {
        accum.batches.pop().expect("length checked")
    } else {
        arrow_select::concat::concat_batches(schema, accum.batches.iter()).map_err(|e| {
            re(format!(
                "match_recognize: coalescing shard rows failed: {e}"
            ))
        })?
    };
    accum.batches.clear();
    accum.bytes = 0;
    writers.append(shard, &merged, index)
}

/// A batch holding just `rows`, in that order.
fn take_rows(batch: &RecordBatch, rows: &[u32]) -> Result<RecordBatch> {
    let idx = arrow_array::UInt32Array::from(rows.to_vec());
    let cols = batch
        .columns()
        .iter()
        .map(|c| arrow_select::take::take(c.as_ref(), &idx, None))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| {
            re(format!(
                "match_recognize: sharding a buffered batch failed: {e}"
            ))
        })?;
    RecordBatch::try_new(batch.schema(), cols).map_err(|e| {
        re(format!(
            "match_recognize: rebuilding a sharded batch failed: {e}"
        ))
    })
}

/// Fold a value into a hasher.
///
/// Only has to be consistent within one `split` pass, so the encoding is chosen for
/// simplicity — but NULL is given its own tag so it cannot collide with a real value,
/// and floats hash by bit pattern so `-0.0` and `0.0` land together only if they
/// compare equal (they do not, bitwise, which merely costs an extra shard entry, never
/// a wrong grouping — rows are regrouped by the real comparator inside the shard).
fn unit_tag(u: &mr_core::types::TimeUnit) -> u8 {
    use mr_core::types::TimeUnit::*;
    match u {
        Second => 0,
        Milli => 1,
        Micro => 2,
        Nano => 3,
    }
}

fn hash_value(v: &Value, h: &mut DefaultHasher) {
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        Value::Int(i) => {
            2u8.hash(h);
            i.hash(h);
        }
        Value::HugeInt(i) => {
            3u8.hash(h);
            i.hash(h);
        }
        Value::Double(d) => {
            4u8.hash(h);
            d.to_bits().hash(h);
        }
        Value::Decimal(v, s) => {
            5u8.hash(h);
            v.hash(h);
            s.hash(h);
        }
        Value::Str(s) => {
            6u8.hash(h);
            s.hash(h);
        }
        Value::Date(d) => {
            7u8.hash(h);
            d.hash(h);
        }
        // The unit is folded in as a small integer, not `format!("{u:?}")` — that was a
        // heap allocation per row for a timestamp partition key, on this loop.
        Value::Timestamp(t, u) => {
            8u8.hash(h);
            t.hash(h);
            unit_tag(u).hash(h);
        }
        Value::Time(t, u) => {
            9u8.hash(h);
            t.hash(h);
            unit_tag(u).hash(h);
        }
        Value::Interval(i) => {
            10u8.hash(h);
            i.months.hash(h);
            i.days.hash(h);
            i.nanos.hash(h);
        }
        Value::List(items) => {
            11u8.hash(h);
            items.len().hash(h);
            for it in items {
                hash_value(it, h);
            }
        }
    }
}
