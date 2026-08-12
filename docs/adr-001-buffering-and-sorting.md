# ADR 001 — How we buffer and sort, and why not an embedded DuckDB

**Status:** accepted · **Date:** 2026-08-12

## Context

`match_recognize` cannot stream its input: a match may span an entire partition, so
a partition has to be complete before it can be matched, and DuckDB does not hand
the function its input clustered by partition key. So the worker buffers the whole
relation, groups it into partitions, sorts each partition, and matches.

Today that means: `process` writes each (projected) batch to the SDK's
cross-process store as Arrow IPC — a SQLite file under `$TMPDIR` — and
`finalize_producer` reads it all back into memory, builds one `usize` tape per
partition, sorts each tape, and streams matches out a partition at a time.

The question raised: since we are buffering anyway, should we buffer into a
**DuckDB file** instead, and let DuckDB do the sorting and partitioning — getting
its spilling, parallel sort and an input size not bounded by RAM?

## Measurements

All on an 8-core (4 performance) M-series laptop, release build.

| | |
|---|---|
| our sort, 4M rows, BIGINT key | ~2.4 s of a 3.96 s query |
| **DuckDB sorting the same 4M rows** | **0.039 s** |
| our sort, 1.6M rows, in isolation | 677 ms (BIGINT), 795 ms (VARCHAR) |
| our partitioning, 1.6M rows | 269 ms (~170 ns/row) |
| peak worker RSS, 5M rows × 4 columns | ~0.6 GB (~120 bytes/row resident) |
| 4M-row query, input pre-sorted by the host | **3.96 s → 1.52 s** |

Two things follow. DuckDB's sort is roughly **60× faster** than ours, and it is
already available to us: adding `ORDER BY` to the input subquery cut a 4M-row query
by 2.6×, which means DuckDB preserves that order into the table function and our
sort then sees an already-ordered run.

## Decision

**Do not embed DuckDB.** Take the host's sort where it is offered, and keep our own
as the correctness floor.

### Why not embed

- **Binary size**: bundled libduckdb is ~47 MB against a 15 MB worker.
- **It breaks the wasm target.** The browser build cannot compile bundled C++, so
  this would have to be a cargo feature — and then the *maximum input size* of the
  function differs by build. A SQL surface that changes shape depending on how it
  was compiled is a bad surface.
- **Engine inside engine.** DuckDB launches the worker, which embeds a second
  DuckDB: engine init on every `ATTACH`, two buffer pools, two versions to keep in
  step.
- **It trades our semantics for someone else's, invisibly.** Sorting semantics are
  where this project's bugs have actually been: an i64 overflow that reversed
  `ORDER BY` past year 2262, a NULL placement flipped by `DESC`, an intransitive
  comparator on NaN. Those were found by a differential test that pins two
  comparators together (`mr-worker/tests/sort_agreement.rs`). Delegating to a
  bundled engine replaces them with whichever semantics that version has, which is
  not obviously better and is *harder* to pin, because the reference moves when the
  dependency moves.
- **It does not remove the ceiling on its own.** The relation would still be read
  back to match it, unless we also restructure finalize to stream partitions — and
  if we do that restructuring, we no longer need the embedded engine for it.

### What we do instead

**Now (no code).** Document that pre-sorting the input subquery makes our sort
nearly free. It is safe by construction: we sort regardless, so a host that does
not preserve order costs performance, never correctness.

**Next, if inputs outgrow RAM.** Stream partitions at finalize instead of
materializing the relation: when the buffered input is ordered by
`(partition keys…, order keys…)`, read pages from the store until the partition key
changes, match that partition, emit, discard. Memory then tracks the largest
partition rather than the whole input. This needs the ordering *verified* while
streaming — a non-decreasing check on the partition key — with a fall back to the
current path if it does not hold, because splitting one partition across two
streamed chunks would silently produce different matches.

**Only if that is not enough.** An external merge sort of our own over the buffered
batches (spill sorted runs to the same store, k-way merge), for inputs that are
both larger than RAM *and* unsorted. Bounded work, no new dependency.

## Addendum — the sink-side ordering knobs

The SDK exposes three, and it is worth recording why we set only one.

`FunctionMetadata.order_preservation` describes what our *output* does. We declare
`NO_ORDER_GUARANTEE`, which is the truth: rows come out partition by partition, in
whatever order the partitions arrived.

The other two are mutually exclusive (the extension's header says so) and both
concern the *input*:

- `requires_input_batch_index` hands us DuckDB's per-chunk batch index, giving us the
  true input order — enough to make tie ordering deterministic and to let a future
  finalize stream partitions. **We declare it** (`catalog.rs`); `buffer.rs` sorts the
  buffered batches by it, and `test/sql/batch_index.test` pins the result: among rows
  tying on the `order_by` key, output order is input order. Flipping the flag off
  makes that test fail, which is how we know the flag is what buys it.

  Declaring it used to be impossible: DuckDB asserts
  `pipeline.source->SupportsPartitioning(BatchIndex())` in `PipelineExecutor`'s
  constructor, so such a query died before any extension code ran — an
  InternalException in a debug build, and a **segfault** (exit 139, no error, no
  output) in release, where the assert compiles out. **Fixed upstream** in the vgi
  extension (`table_buffering: don't crash when the source cannot supply a
  batch_index`): the plan now checks the pipeline source and, when it cannot supply
  an index, serializes the sink and numbers the batches itself, so the worker gets a
  valid monotonic index from any source and no caller has to wrap their input.

  The one cost worth naming: on a source that cannot supply an index the sink is
  serialized, so ingest — ~83% of a realistic query's wall time — loses its
  parallelism. Pre-materializing such input (a temp table, a parquet scan) restores
  it. Sources that can supply an index, which is the interesting case for large
  inputs, keep the parallel sink.
- `sink_order_dependent` forces `ParallelSink=false` unconditionally. It works with
  any source, but it pays that serialized-ingest cost even where a batch index was
  available, so it buys nothing the flag above does not.

## Consequences

- Input size is bounded by memory at roughly 120 bytes per row until the streaming
  path above is built. That is the honest limit, and it is documented in the README
  rather than implied.
- We keep owning the comparator, so we keep owning its bugs — which is the point of
  the differential and adversarial-distribution tests around it
  (`mr-core/tests/sorting.rs`, `mr-worker/tests/sort_agreement.rs`).
- Users with large inputs get most of the available win for free, by writing
  `ORDER BY` in the subquery they were already writing.
- Row order among ties on the `order_by` key is **input order**, and stable between
  runs. SQL:2016 leaves it implementation-defined; we pin it, because the batch index
  makes that free. On a source that cannot supply an index the price is a serialized
  sink — correctness never varies, only ingest throughput.
