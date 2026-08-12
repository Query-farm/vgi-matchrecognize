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

/// Ceiling on shards. Each is a file and a finalize stream, and past this the split
/// costs more than the memory it saves.
const MAX_SHARDS: usize = 64;

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

/// Rewrite an execution's spooled batches into `shards` shard files, keyed by
/// partition key, and delete the originals.
///
/// Streams one batch at a time — the point is to bound memory, so the pass itself must
/// not materialise the relation. Rows of one input batch that land in the same shard
/// are written together, so a shard file holds whole (sub-)batches, each still carrying
/// its original batch index so input order can be restored per shard.
pub fn split(
    scope: &[u8],
    plan: &Plan,
    projected_schema: &SchemaRef,
    shards: usize,
) -> Result<Vec<u64>> {
    debug_assert!(shards > 1, "splitting into one shard is a no-op");
    let mut rows_per_shard = vec![0u64; shards];
    let part_cols = plan.partition_by_columns();
    let paths = crate::spool::files_with_prefix(scope, "sink-");
    for path in &paths {
        for (index, batch) in crate::spool::read_files(std::slice::from_ref(path))? {
            // Column positions are resolved per batch against the schema the sink
            // wrote, which is the projected one.
            let store = BatchRowStore::new(projected_schema.clone(), vec![batch.clone()]);
            let cols: Vec<usize> = part_cols
                .iter()
                .map(|c| {
                    mr_core::engine::RowStore::col_index(&store, c).ok_or_else(|| {
                        re(format!("match_recognize: unknown partition column '{c}'"))
                    })
                })
                .collect::<Result<_>>()?;

            // Row indices per shard, then one `take` per shard: a single pass over the
            // batch and one copy of each row.
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
                crate::spool::append_shard(scope, s, &sub, index)?;
            }
        }
    }
    crate::spool::remove_sink_files(scope);
    Ok(rows_per_shard)
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
        Value::Timestamp(t, u) => {
            8u8.hash(h);
            t.hash(h);
            format!("{u:?}").hash(h);
        }
        Value::Time(t, u) => {
            9u8.hash(h);
            t.hash(h);
            format!("{u:?}").hash(h);
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
