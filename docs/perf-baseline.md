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
