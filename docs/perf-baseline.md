# Performance baseline

Numbers printed by `crates/mr-core/tests/perf_probe.rs`, which is the tool to run
before and after touching any phase:

```sh
cargo test --release -p mr-core --test perf_probe -- --ignored --nocapture \
    --test-threads=1
```

`--test-threads=1` matters: the probes are CPU-bound, and running them concurrently
inflated one sweep by 60%.

**Machine / build:** Apple M-series (8 cores, 4 performance), macOS 15.6, release
profile `opt-level = 3`, `lto = "thin"`. Absolute numbers are machine-specific — the
point of this file is the *shape* and the deltas.

## Baseline — 2026-08-12, commit `ed1ab20` (before any optimisation)

### Cost per row vs match length (one match of L rows)

The column to read is **ns/row/L**. Flat and non-zero means the phase is linear in
the match, so the match as a whole is quadratic. Flat and ~0 is the goal.

| Shape | L=1,000 | L=4,000 | L=16,000 | L=32,000 | ns/row/L |
|---|---|---|---|---|---|
| control `B: k >= 0` (ONE ROW) | 175 | 146 | 130 | 121 ns/row | ~0.00 |
| **`B: k >= A.k`** (ONE ROW) | 3,098 | 9,845 | 30,344 | 60,464 ns/row | **1.9** |
| control `RUNNING COUNT(*)` (ALL ROWS) | 123 | 123 | 121 | — | ~0.01 |
| **`RUNNING SUM(k)`** (ALL ROWS) | 14,344 | 64,132 | 230,845 | — | **14.4** |
| **`LAST(k)`** (ALL ROWS) | 6,656 | 28,859 | 109,619 | — | **6.9** |
| **`FINAL SUM(k)`** (ALL ROWS) | 27,208 | 111,434 | 455,845 | — | **28.5** |

`FINAL SUM` is the worst and is pure waste: its value is constant across the match's
output rows, and it is recomputed over every bind for each one.

Extrapolating the 1.9 ns/row/L line, a single 1M-row match with one `A.k` reference
costs ~1.9 ms **per row** — about half an hour for the match. That is the shape that
makes "millions of rows" false today for anything but the simplest predicates.

### Matcher + measures (1M rows, 1 partition, pre-ordered)

| Shape | ns/row |
|---|---|
| never matches (VM floor) | 64 |
| `A` — one match per row | 142 |
| `A+` — one match, ONE ROW, `count(*)` | 84 |
| `A+` — one match, ALL ROWS, `count(*)` | 142 |
| v-shape, 3 vars, `PREV` predicates | 105 |

### Input-scaling phases

| Phase | 100k | 400k | 1.6M |
|---|---|---|---|
| sort, BIGINT key, scrambled | 211 | 340 | 587 ns/row |
| sort, VARCHAR key, scrambled | 283 | 509 | 831 ns/row |
| partitioning, 10 rows/partition | 132 | 136 | 167 ns/row |

Pre-ordered 1.6M input: 93 ms either direction (ascending or descending), versus
940 ms scrambled — re-sorting an already-ordered run is nearly free, which is why
`ORDER BY` in the input subquery pays for itself.

### Worker-side buffering round trip (2M rows × 3 BIGINT, 977 chunks)

Measured separately (see ADR 001); not yet part of the probe:

| Step | ns/row |
|---|---|
| Arrow IPC serialise | 4.3 |
| **SQLite `append`** | **93.5** |
| `read_batches` (scan + deserialise + sort) | 17.9 |
| `BatchRowStore::cell` (977 batches) | 9.9 ns/cell, 35% in `locate` |

### End to end, through DuckDB

| Query | threads=1 | threads=8 |
|---|---|---|
| 2M rows, 1000 partitions, `DOWN+` | 0.83 s | 0.80 s |
| 8M rows, 1000 partitions, `DOWN+` | 5.77 s | 3.70 s |

Matching is single-threaded regardless of `threads`, so the only thing that scales is
ingest — which caps the whole query at ~1.5×.

## After the quadratic work — commit `94fa482`

Same machine, same probes. `ns/row/L` is now ~0 for every shape, which is the whole
point: cost per row no longer depends on how long the match is.

| Shape (ns/row) | L=1,000 | L=4,000 | L=16,000 | L=32,000 | ns/row/L | vs before |
|---|---|---|---|---|---|---|
| control `B: k >= 0` | 106 | 74 | 71 | 68 | ~0.00 | — |
| `B: k >= A.k` | 88 | 85 | 84 | 85 | **0.00** | **600×** at L=32k |
| control `RUNNING COUNT(*)` | 162 | 159 | 173 | — | ~0.01 | — |
| `RUNNING SUM(k)` | 201 | 190 | 188 | — | **0.01** | **1,230×** at L=16k |
| `LAST(k)` | 137 | 137 | 132 | — | **0.01** | **830×** at L=16k |
| `FINAL SUM(k)` | 163 | 156 | 157 | — | **0.01** | **2,900×** at L=16k |

A qualified reference now costs the same as reading a plain column, and a running
aggregate costs about what `COUNT(*)` costs. Extrapolating the old 1.9 ns/row/L line,
a 1M-row match with one `A.k` reference went from ~30 minutes to roughly 0.1 s.

Matcher throughput over the same period (1M rows, ns/row), which is where the
allocation work shows up:

| Shape | baseline | now |
|---|---|---|
| never matches (VM floor) | 64 | **52** |
| `A` — one match per row | 142 | 131–164 |
| `A+` — one match, ONE ROW | 84 | **65** |
| `A+` — one match, ALL ROWS | 142 | **135** |
| v-shape, `PREV` predicates | 105 | 108 |

Two regressions were found and fixed while measuring rather than shipped: building the
per-label index fresh per match cost 36% on a partition of many tiny matches (fixed by
reusing one index per partition), and memoizing `COUNT(*)` — already O(1) — added two
hash lookups per output row (fixed by not memoizing it). The v-shape is ~3% down on
the index's per-bind push cost, which interning labels should return.

## Final — 2026-08-12, all workstreams landed

Same machine, same probes, `--test-threads=1`.

### End to end, through DuckDB (1000 partitions, `DOWN+`)

| Query | before | after | |
|---|---|---|---|
| 2M rows, threads=1 | 0.83 s | **0.28 s** | 3.0× |
| 2M rows, threads=8 | 0.80 s | **0.19 s** | 4.2× |
| 8M rows, threads=1 | 5.77 s | **1.19 s** | 4.8× |
| 8M rows, threads=8 | 3.70 s | **0.97 s** | 3.8× |
| 8M rows, peak worker RSS | 365 MB | **167 MB** (32 MB budget) | 2.2× |

Threads now help rather than being the only thing that helps: matching is parallel,
and ingest no longer dominates.

### Compute phases

| Phase | before | after | |
|---|---|---|---|
| matcher VM floor (1M rows) | 64 | **38** ns/row | 1.7× |
| `A+` one match, ONE ROW | 84 | **45** ns/row | 1.9× |
| `A+` one match, ALL ROWS | 142 | **118** ns/row | 1.2× |
| v-shape, `PREV` predicates | 105 | **69** ns/row | 1.5× |
| sort, BIGINT key, 1.6M scrambled | 587 | **204** ns/row | 2.9× |
| sort, VARCHAR key, 1.6M scrambled | 831 | **679** ns/row | 1.2× |
| partitioning, 1.6M / 160k partitions | 167 | **96** ns/row | 1.7× |
| re-sorting 1.6M pre-ordered rows | 93 ms | **55 ms** | 1.7× |

### Cost per row vs match length — the quadratics

Every shape is flat now (`ns/row/L` ~0), so cost per row no longer depends on how
long the match is:

| Shape | before, L=16k | after | factor |
|---|---|---|---|
| `B: k >= A.k` (qualified ref) | 30,344 | **80** ns/row | 380× |
| `RUNNING SUM(k)` | 230,845 | **139** ns/row | 1,660× |
| `FINAL SUM(k)` | 455,845 | **~150** ns/row | ~3,000× |
| `LAST(k)` | 109,619 | **~120** ns/row | ~910× |
| `SUM(k)` inside DEFINE | 231,831 | **~130** ns/row | ~1,780× |

At L=32k the qualified-reference case went from 60,464 to 85 ns/row (600×).
Extrapolated, a 1M-row match with one `A.k` reference went from roughly half an hour
to about 0.1 s.

## Sharded finalize: what it costs

The one change that is a trade rather than a win. 8M rows, 2000 partitions, interleaved
A/B runs:

| Budget | shards | wall clock | peak worker RSS |
|---|---|---|---|
| 256 MB (default) | 1 — no split | **1.00–1.27 s** | 365 MB |
| 32 MB | ~6 | 2.15–2.98 s | **167 MB** |
| 8 MB | ~24 | 3.8–5.3 s | — |

So ~2× the time for ~2× less memory, and it degrades as shards multiply. The split is a
second full pass over the data and it is serial — DuckDB's own CPU time *drops* (1.54 s
→ 0.29 s) while wall clock rises, i.e. it is waiting on the worker. Hashing is not the
bottleneck: replacing the per-row `Value` with a typed key reader changed nothing
outside noise, and was reverted.

This is why the default budget is high: sharding is for the query that would otherwise
run out of memory, not for throughput. Making it cheap would mean sharding at *sink*
time (no extra pass, but a per-row cost on every query, whether it needs it or not).

## How far it scales

Measured, not extrapolated: 100M rows, 3 BIGINT columns, 100k partitions of 1000 rows,
`DOWN+ UP+`, on the 24 GB machine above.

| | time | peak worker RSS | peak spool on disk |
|---|---|---|---|
| unsharded (budget raised) | **17.8 s** | 2.6 GB | 2.4 GB |
| sharded (256 MB default → 10 shards) | 44.5 s | **0.7 GB** | 3.0 GB |

Scaling is linear through this range: 8M → 1.0 s, 100M → 17.8 s (12.5× rows, 12.5×
time), and resident memory is ~26 bytes/row unsharded. So a **billion** rows of this
shape extrapolates to roughly 3 minutes unsharded (needing ~26 GB of RAM, i.e. more
than this machine has) or 7–8 minutes sharded at ~1–2 GB, with ~24 GB of spool.

What actually limits it, in the order it bites:

1. **One giant partition cannot be divided.** No `partition_by`, or one partition
   holding most of the rows, means the whole relation is resident and sharding cannot
   help. This is inherent: a match may span the partition, so the partition is the
   smallest unit that can be matched soundly.
2. **Disk.** The spool is ~24 bytes/row. Peak was twice the relation until each sink
   file started being deleted as the split consumed it; it is now the relation plus
   about one file (5.0 → 3.0 GB on the 100M query).
3. **Time**, because the split pass is serial — ~2.5× here, and DuckDB idles through it.
4. **The shard ceiling**, raised from 64 to 1024 for exactly this reason: at 64 the
   budget stopped binding above 64 × 256 MB = 16 GB of input, and peak memory went back
   to growing with the relation. At 1024 the default budget holds to ~256 GB.

Index widths are not a limit at this scale: tape positions, step budget, match numbers
and batch indices are all `i64`, and the `u32` bind indices only cap a single *match* at
4.29B rows.

## Measured and declined

Two things the plan proposed that measurement argued against. Recorded so they are not
re-attempted blind:

- **Resolving column names to integer ids.** A profile attributed 4.3% of matcher time
  to `col_index`, but that profile predates label interning. Two experiments since: an
  O(1) `HashMap` lookup measured *worse* than the existing linear scan (v-shape 108 →
  133 ns/row — hashing a name costs more than three short `eq_ignore_ascii_case`
  compares), and an artificially cheap lookup (length-only compare, incorrect, purely
  to measure the ceiling) measured the same or worse. Projection pushdown keeps the
  store to the columns the pattern reads, so the scan is over ~3 names. Integer ids
  would need `Expr::Col`/`Expr::Qualified` churn for no measurable gain.
- **`opt-level = 3` without LTO.** Slower than `opt-level = 2` on every shape; see
  below.

## Compiler-flag experiment

`opt-level` and LTO, measured on `perf_matcher` (two runs each, single-threaded):

| Profile | VM floor | `A` per row | `A+` ONE ROW | `A+` ALL ROWS | v-shape |
|---|---|---|---|---|---|
| `opt-level = 2` (previous) | 64 | 139–141 | 74–90 | 140–149 | 112–113 |
| `opt-level = 3` alone | 90 | 193 | 93 | 156 | 126 |
| **`opt-level = 3` + `lto = "thin"`** | **59** | **131** | **72–74** | **134–135** | **103–104** |

So `opt-level = 3` *on its own* is worse than `2` — inlining without cross-crate
visibility, presumably — and only pays off with thin LTO, which then gives a
consistent 6–8%. Adopted. The cost is link time: a full release build goes from
~35 s to ~1 m 40 s, which CI pays once per job.
