# CLAUDE.md

Guidance for working in this repository.

## What this is

`vgi-matchrecognize` is a **VGI worker** (a standalone binary DuckDB launches and
talks to over Apache Arrow IPC, `ATTACH 'mr' (TYPE vgi, LOCATION '…')`) that
brings **SQL:2016 `MATCH_RECOGNIZE` row pattern matching** to DuckDB, which has
no native support for it. Functions live under catalog `mr`, schema `main`.

Built on the published VGI Rust SDK (`vgi = "0.29"` from crates.io), arrow 59.
The repo builds standalone — no local SDK checkout, no path deps except the
intra-workspace `mr-core`. License **MIT**.

## SQL surface

- `mr.main.match_recognize((<relation>), partition_by:=, order_by:=, pattern:=,
  define:=, measures:=, rows:=, after:=, [step_budget:=])` — the one real
  function: a **table-in / table-out buffering function**. The relation is a
  subquery (NOT a correlated `LATERAL`); everything else is a scalar `const_arg`
  (named). `partition_by`/`order_by` are `VARCHAR[]`; `pattern`/`rows`/`after`
  are `VARCHAR`; `define`/`measures` are JSON strings.
- `mr.main.explain_pattern(p)` — pretty-print a compiled pattern; no data.
- `mr.main.after_match_skip_modes` — a browsable reference view of the AFTER
  MATCH SKIP modes the `after` argument accepts (inline `VALUES`, no data access).

The worker build version is published as the catalog's `implementation_version`
(readable from `vgi_catalogs()`), per VGI328 — there is no `*_version()` scalar.

## Architecture — two crates

A Cargo **workspace** mirroring `../vgi-fixedformat`:

- **`crates/mr-core`** — PURE compute, **no Arrow / no VGI** (`unsafe` forbidden),
  the bulk of correctness, unit- and proptest-tested with an in-memory store:
  - `pattern/` — `lexer` → `parser` (the `Pattern` AST: concat / alternation /
    quantifiers / grouping / anchors) → `compile` (a backtracking-VM `Program`
    of `Char`/`Split`/`Jmp`/`Anchor`/`Match` instructions; greedy vs reluctant
    differ only in `Split` branch order) + `explain`.
  - `expr/` — `lexer` → a Pratt `parser` → the `Expr` AST shared by DEFINE and
    MEASURES.
  - `types/` — `ty` (the `Ty` enum, 1:1 with emitted Arrow `DataType`s) +
    `infer` (spec §C bind-time type synthesis; the densest test target).
  - `engine/` — `rowstore` (the Arrow-agnostic `RowStore` trait + a `VecRowStore`
    for tests), `eval` (the `Frame`: bindings, RUNNING/FINAL horizon,
    PREV/NEXT/FIRST/LAST, running aggregates, 3-valued NULL logic), `valops`
    (arithmetic / comparison / coercion), `matcher` (the backtracking VM + step
    budget + AFTER MATCH SKIP).
  - `plan` — `Plan::build` (bind: parse + type-check + compute the output column
    layout) and `Plan::run` (produce: group → sort → match → evaluate → rows).
    `Plan::partition_tapes` + `Plan::run_partition` are the streaming form of
    `run`: one partition at a time, threading the match number across calls.
- **`crates/mr-worker`** — thin Arrow/VGI adapter:
  - `match_recognize.rs` — the `TableBufferingFunction` (`on_bind` / `process` /
    `combine` / `finalize_producer`); spools each batch, then builds the `Plan` and
    returns a `PartitionStream` producer that matches a *chunk* of partitions on
    several threads, emitting them in partition order in ~8k-row batches.
  - `spool.rs` — the buffered batches: one append-only Arrow-IPC file per sink thread
    under `$TMPDIR`, bypassing the SDK store (which cost 93.5 ns/row against 4.3 to
    serialise). One `write()` per `process()` call — there is no end-of-input hook to
    flush a userspace buffer at. Records are LZ4-compressed once the sink has written
    32 MB, when asked (off by default — see the knobs table). **Compress the whole
    record, never via arrow's `IpcWriteOptions`**: arrow compresses per *buffer* (per
    column, per batch), and the split's ~205-row sub-batches made that catastrophic —
    346-507 ns/row to read shards back, against 20 frame-level.
    A record is a **64-byte header** (`batch_index | stored_len | raw_len | codec`, padded)
    then the payload, then padding to the next 64. Both paddings earn their keep: arrow
    aligns each buffer 64 bytes *relative to the payload start*, so a payload at an
    arbitrary file offset has every buffer misaligned and a reader must copy all of it —
    with the padding, `read_files` decodes through arrow's `StreamDecoder` over an aligned
    `MutableBuffer` and the arrays borrow it. That took read+decode from 16 to **5
    ns/row**. `raw_len` is what the shard count divides by the memory budget; file sizes
    stopped predicting the producer's peak the moment anything was compressed.
    Files are **mmap'd** and an uncompressed record's arrays borrow the mapping — reading
    is then pointer arithmetic (1-3 ns/row, and 0-1 for shard files) and the resident set
    is file-backed, so a producer holding a whole shard can be paged rather than having to
    fit in anonymous memory. Which is why **shard records are never compressed** even when
    sink records are: borrowing beats a decompression pass into heap. Mapping is sound only
    because a spool file is complete before it is mapped and is never truncated. **Mapping is
    unix-only**: Windows refuses to delete a file that has a mapping open at all
    (`ERROR_USER_MAPPED_FILE`), and cleanup here unlinks files *while* their records are being
    read, so there a cursor reads through a buffer and pays the copy. Anywhere a file is
    deleted to reclaim space, the cursor and any batch decoded from it must be dropped first
    (`RecordCursor::into_path`) — unlinking a still-mapped file frees nothing.
  - `shard.rs` — splits the spool by partition key when it exceeds the finalize memory
    budget, so peak memory tracks a shard rather than the relation. It **merges the sink
    files in global batch-index order**, which is not optional: sink files carry strided
    indices (one thread wrote 0, 8, 16…, another 1, 9, 17…), so coalescing records without
    merging would make every row of one sink sort before every row of the next and tie
    order under `order_by` would depend on DuckDB's scheduling. Records are coalesced to
    ~256 KB, which took a 10-shard split from 48,830 records to 964; without it, 1024
    shards would write two-row records and the shard files would be ~10x the input. **A trade, not a
    win**: measured 2× wall clock for 2× less memory (the split is a second full,
    serial pass), so the default budget is high enough that ordinary queries never take
    it. Do not "optimize" it expecting a speedup — hashing was measured and is not the
    bottleneck. It also costs **peak disk of the spool plus the shards** (~1.5x the relation,
    measured), and that is not fixable by deleting sinks sooner: the merge consumes strided
    indices in global order, so every sink reaches its last record at about the same time.
    Bounding it would take segmenting each sink and chaining the segments behind one cursor.
  - `arrow_in.rs` — a `RowStore` over the buffered `RecordBatch`es, addressed as
    one contiguous row space (deliberately **not** concatenated — a merged copy
    would double peak memory for no gain).
  - `arrow_out.rs` — `Vec<Vec<Value>>` + output `Ty`s → a `RecordBatch`.
  - `schema.rs` — `Ty` ↔ Arrow `DataType` + the `ArrowBindSchema` for inference.
  - `scalar/` — `explain_pattern`.
  - `catalog.rs` / `meta.rs` — catalog/schema/function metadata for `vgi-lint`.
  - `main.rs` registers everything and calls `Worker::run()`.

## The two hard design points

1. **Buffer-all, then compute.** Row pattern matching is intrinsically a
   whole-partition operation, so `match_recognize` is a `TableBufferingFunction`
   (Sink+Source), the `vgi-match` idiom: `process` spools each Arrow batch to disk
   (`spool.rs`); `finalize_producer` reads it back and streams the result. The
   partition is the smallest sound unit of work: a match may span a whole partition,
   so a partition must be complete before it can be matched — but partitions are
   independent, so a chunk of them is matched concurrently, and when the relation is
   too big for the memory budget `combine` splits it into per-partition-key shards
   and returns one finalize state per shard.
2. **Output schema is fixed at `on_bind`** — before any data flows. The measure
   types are **inferred statically** from `params.input_schema` + the parsed
   measure ASTs (`mr-core::types::infer`), with an explicit `{"as","expr","type"}`
   override escape hatch for anything inference can't decide.

**Do not "just embed a DuckDB"** in the worker to buffer and sort. It was considered and
rejected: bundled libduckdb is ~47 MB against a 15 MB worker; it cannot compile for wasm,
so it would have to be a cargo feature, and then the function's maximum input size would
differ by build — a SQL surface that changes shape depending on how it was compiled. It
also puts an engine inside an engine (init per `ATTACH`, two buffer pools, two versions to
keep in step) and silently trades our sorting semantics for whichever ones that version
has. Sorting is exactly where this project's bugs have been (an i64 overflow that reversed
`ORDER BY` past 2262, NULL placement flipped by `DESC`, an intransitive comparator on
NaN), all caught by `mr-worker/tests/sort_agreement.rs` pinning two comparators together —
a moving reference is harder to pin, not easier. And it would not lift the memory ceiling
by itself, since the relation still has to be read back to match it.

## Conventions / gotchas

- All algorithms live in `mr-core` with unit + property tests; the worker is a
  thin adapter. The pure core is testable with `VecRowStore` — no IPC, no DuckDB.
- Logs go to **stderr** — stdout is the Arrow-IPC channel.
- The catalog name must match the ATTACH name; `main.rs` defaults
  `VGI_WORKER_CATALOG_NAME` to `mr`.
- `serde_json` is built with `preserve_order` so a MEASURES **object**'s key
  order is the output column order (the SDK contract).
- **Labels are integer ids** (`engine/labels.rs`, `VarId`), assigned at bind time:
  pattern variables in declaration order, then subset names. `Inst::Char`, `Bind`,
  `AfterSkip` and `BindIndex` all carry ids, `define` is a `Vec` indexed by id, and
  `Bind` is `Copy`. Carrying label *strings* here cost an allocation per VM step (~47%
  of matcher self time in malloc). Expressions keep written labels and resolve them by
  exact compare — cheaper than hashing, and once per node rather than per step.
- **Three things that were quadratic in match length, and the shape of each fix.** All
  are pinned by `perf_probe.rs::perf_match_length`, whose `ns/row/L` column must stay
  ~0, and by `tests/running_aggregates.rs` + `tests/bind_index.rs` for values:
  - `LAST(A.x)` (which is what a bare `A.x` means) scanned the match backwards →
    `engine/bindindex.rs` keeps per-label ascending bind indices, maintained
    incrementally by the matcher and **truncated in lockstep with every**
    `binds.truncate()`. A stale entry is a wrong answer, not a slow one.
  - A running aggregate re-folded its whole scope per output row → `engine/aggmemo.rs`
    extends the fold instead, keyed by the address of the `Expr::Agg` node. Only sound
    while each row's contribution is horizon-independent, so `memoizable` is a
    conservative gate: a qualified reference is accepted **only when it is the
    dominant qualifier** (otherwise `SUM(A.v + B.v)` freezes a stale `LAST(B)`), and
    the matcher clears the memo wherever `binds` shrinks.
  - `FIRST`/`LAST` materialised the scope as a `Vec<Bind>` per call → `scope_nth`
    answers "n-th from this end" without building anything.
- The matcher is a backtracking VM over an **explicit heap stack** of pending
  alternatives (`Alt { ip, pos, binds_len }`), never host recursion — match length
  must not be bounded by the OS stack (it once was, and a match over ~8k rows
  aborted the process). `binds` is append-only along a path, so restoring an
  alternative is just `binds.truncate(binds_len)`, and popping LIFO reproduces the
  greedy/reluctant preference order that `Split` branch order encodes.
  Termination is guaranteed two ways: the per-partition **step budget** (no hang)
  bounds inner work, and the outer match loop advances the tape cursor by ≥1 row
  each iteration.
- `PlanConfig::step_budget` is `Option<i64>`: `None` means `auto_step_budget(rows)`
  (128 steps/row, floor 5M), computed per partition in `run_partition`. The budget
  targets *super-linear* backtracking, so it has to scale with the partition — a
  constant default cut off ordinary linear matches past ~1.5M rows.
- `Frame::var_at_tp` **binary searches** `binds` (which is strictly increasing in
  `tape_pos`, since matches consume rows left to right). It is on the PREV/NEXT hot
  path; a linear scan there made `x <= PREV(x) + 1` quadratic in match length.
  `last_bind_of` / `scope` are still linear in the match — fine for short matches,
  a known cost for very long ones.
- Both parsers cap nesting at 128 levels (`MAX_DEPTH`) — they recurse through
  grouping, and `pattern`/`define`/`measures` are user-supplied strings.
- A bare qualified ref `A.col` outside a navigation/aggregate call means
  `LAST(A.col)` under the prevailing RUNNING/FINAL semantics (NULL if `A` is
  unbound); `eval` reads it directly only when the row being evaluated is
  **covered by** the label — `Frame::label_covers`, which is subset-aware. An
  equality test there silently broke `array_agg(U.col)`, since a member variable
  is not equal to the union's name.
- SUBSET union variables live in `Frame::subsets`; a label matches a bound row if
  it is the same variable or a subset listing it. `CLASSIFIER` resolves from the
  tape (`var_at_tp`), not from `cur_var`, so it stays right when a navigation pins
  `cur_var` to a qualifier.
- Labels are canonical: unquoted -> UPPER, double-quoted -> as written, and
  comparisons are then **exact**. `plan::resolve_var` maps a written name (a JSON
  key or an expression reference) to the canonical one, exact match first.
- `Ty` is NOT `Copy` (it owns `List`'s element type); clone it.
- The worker buffers only `Plan::referenced_columns()`. Buffering volume dominates
  runtime — one unused 200-byte column measured 2.8x on 2M rows.
- **Sorting reads fixed-width keys out once** (`plan.rs::sort_tape_on_keys`) instead
  of calling `cmp_cells` per comparison, which re-located the row in the store every
  time. Integer-family keys are packed as `i64` + a null flag; VARCHAR/LIST stay on
  `cmp_cells` (materialising them would copy the column). Skipped below 256 rows —
  the allocations lost 11% on a query with 160k tiny partitions. A temporal value whose
  unit disagrees with its column's declared type falls back, since comparing raw
  integers across units is the bug that sorted year 9999 before 2020.
- **The spool is deleted when finalize has read it**, not on `Drop`: the SDK holds the
  producer until a *best-effort* destructor RPC, so `Drop` leaked one directory per
  query. A query killed earlier is caught by the TTL sweep on first spool use.
- **A sharded run must not have one producer delete the shared directory** — the first
  to finish took the others' data with it (289,781 rows became 111,258). Each removes
  only its own shard; whoever is last removes the directory. And because sharding
  bypasses the row-count log, each finalize state carries the row count its shard
  should hold.
- `mr-worker/src/buffer.rs` owns the scan-cursor convention for the SDK-store
  *fallback* path (wasm, an unwritable temp dir). `scan` filters
  `id > after_id` and the contract only says ids are *monotonic*, not where they
  start: SQLite's log is AUTOINCREMENT (first id 1), the fs store's is `max_id + 1`
  on an empty dir (first id 0). Paging from `0` silently dropped the first batch on
  fs. `tests/storage_probe.rs` round-trips the two durable local backends
  (`sqlite`, `fs`) through the real helper; `memory` is refused and `http` is not
  compiled in.
- `append` reports failure by returning a negative id (SQLite returns -1 without
  storing), so `buffer::append_batch` checks it rather than discarding it.
- `combine` encodes the sink count into the finalize state id (`FinalizeState`,
  a 4-byte LE prefix before the scope). The SDK treats that id as opaque, so this
  carries the count *outside* the store: if sinks ran and finalize reads back no
  batches, the phases are not sharing state and we error instead of returning an
  empty result. That backstops the `VGI_WORKER_SHARED_STORAGE=memory` bind check
  without depending on a backend name.
- Empty matches (zero bound rows) ARE reported and DO consume a match number, per
  SQL:2016 — one row positioned on the row the match sits on, with every measure
  evaluated over an empty frame (`CLASSIFIER()`/navigation NULL, `COUNT(*)` 0).
  `empty_matches := 'omit'` drops them under `rows := 'all'`. `Frame` treats
  `binds.is_empty()` as the empty-match marker.
- `MATCH_NUMBER()` counts matches **within a partition**, so `run_partition`
  restarts numbering at 1 for each one.
- Navigation nests: `PREV(LAST(x), n)` anchors on the row `LAST(x)` designates and
  then applies the physical offset (`Frame::nav_anchor`). Resolving the argument
  as a whole would discard the offset.
- A quantified sub-pattern that always matches zero rows (`()*`, `^+`, `(){5,}`)
  is collapsed at parse time to a bounded equivalent — repetition of nothing is
  idempotent — which is what keeps the VM from epsilon-looping.
- An unbounded quantifier (`*`, `+`, `{n,}`) over a **nullable** sub-pattern is
  rejected at compile (it would epsilon-loop); use a bounded form instead.
- `PERMUTE(a, b, …)` is desugared in the **parser** into the alternation of every
  permutation, so the matcher needs no notion of it. Branch order is load-bearing:
  SQL:2016 tries permutations in lexicographic order of the argument positions, which
  is what makes `PERMUTE(A, B)` prefer `A B`. Capped at 6 arguments (720 branches).
- Cross-dialect spellings are accepted where they mean the same thing: `LAG`/`LEAD`
  for `PREV`/`NEXT`, `MATCH_SEQUENCE_NUMBER()` for a RUNNING `COUNT(*)` (both
  Snowflake), `LIST` for `ARRAY_AGG` and `ANY_VALUE` for `ARBITRARY`. Conformance
  suites for Trino, Flink and Snowflake all live in `test/sql/*_conformance.test`;
  the Flink one documents the single case where we deliberately differ.

## Environment knobs

| | |
|---|---|
| `VGI_MR_MATCH_THREADS` | Threads matching partitions. `1` forces the serial path, which is what the determinism checks compare against. Default: machine parallelism, capped at 8. |
| `VGI_MR_FINALIZE_MEMORY_BYTES` | Spooled bytes above which the relation is sharded by partition key. Default 256 MB, at most 64 shards. Small values are how the sharded path gets exercised by hand. |
| `VGI_MR_SPOOL_COMPRESSION` | `lz4` compresses every spooled record, `none` never does. Unset is size-triggered: a sink writes plain until it has written 32 MB *uncompressed*, then switches, so a short query pays nothing. Safe now that the shard count is derived from each record's uncompressed length rather than from file sizes — measuring bytes on disk let compression loosen the memory bound by its own ratio. |
| `VGI_BUFFERING_STORE_TTL_SECS` | Age at which orphaned spool directories are swept (also the SDK store's own knob). Default 24h. |
| `VGI_WORKER_SHARED_STORAGE` | SDK store backend. `memory` is refused off-wasm — the control records must outlive a process. |

## Build & test

```sh
cargo test --workspace                       # unit + proptest + worker integration
cargo test --release -p mr-core --test perf_probe -- --ignored --nocapture \
    --test-threads=1                         # phase timings; see docs/perf-baseline.md
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo doc --no-deps --all-features           # with RUSTDOCFLAGS=-D warnings
cargo build --release                        # the worker binary
./run_tests.sh                               # haybarn SQLLogic e2e (needs the tooling)
```

End-to-end tests need the haybarn tooling (one-time):
```sh
uv tool install haybarn-unittest
echo "INSTALL vgi FROM community;" | uvx haybarn-cli
```
`run_tests.sh` builds the worker and runs `haybarn-unittest` with
`VGI_MATCHRECOGNIZE_WORKER` pointed at the binary. **The `vgi` extension comes from
haybarn**, which has builds the community repository does not — stock DuckDB on Windows gets
a 404 for `windows_amd64`, so the e2e there has to go through `uvx haybarn-cli` /
`haybarn-unittest`.

**Platforms.** CI is Linux only, but the worker was verified by hand on Windows
(`x86_64-pc-windows-msvc`, rustc 1.97.1 — the MSRV): fmt, `clippy -D warnings`, the release
build, all 24 test binaries and the full SQLLogic suite (385 assertions) pass, and a sharded
2M-row query returns bit-identical results to macOS with no spool left behind. Windows needs
MSVC build tools (`Microsoft.VisualStudio.2022.BuildTools` with the VCTools workload) for the
bundled SQLite. Wall clock there ran ~1.7x macOS on the same query.

Metadata gate: `uvx --from vgi-lint-check vgi-lint lint
"$PWD/target/release/vgi-matchrecognize-worker" --fail-on info` → 100/100.
