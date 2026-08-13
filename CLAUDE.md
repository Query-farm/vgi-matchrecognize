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
  define:=, measures:=, rows:=, after:=, [include:=], [step_budget:=])` — the one
  real function: a **table-in / table-out buffering function**. The relation is a
  subquery (NOT a correlated `LATERAL`); everything else is a scalar `const_arg`
  (named). `partition_by`/`order_by`/`include` are `VARCHAR[]`; `pattern`/`rows`/`after`
  are `VARCHAR`; `define`/`subset`/`measures` are declared **`any`** and accept a
  DuckDB `MAP`, a `STRUCT` or a JSON string — `args.rs::structured_arg` normalises
  all three to JSON text so there is one parser and one set of error messages.
  A `MAP` is an ordered entry list, so `measures` key order (= output column order)
  survives. Note what does *not* help there: a `MAP`'s values are still SQL string
  literals, so quoting is only fixed by dollar-quoting (`$$outcome = 'fail'$$`),
  which works on either form.
- `include` is the passthrough escape hatch: SQL:2016 ALL ROWS emits every input
  column, we emit only what the query reads (buffering one unread column measured
  2.8x), so a column you want carried through has to be named. It lands right after
  the partition keys, valued on each matched row under `rows := 'all'` and on the
  match's first row under `'one'`; a column already emitted (a partition key, or an
  order key under ALL ROWS) is dropped rather than duplicated.
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
    The streaming form is `Plan::partitions` → `Plan::match_partition` →
    `Plan::emit_rows(limit)`: `partitions` returns the **CSR** `Partitions` (one
    contiguous `rows` buffer + per-partition `bounds` + labels, built by a counting
    pass, so no per-partition `Vec` and no doubling slack); `match_partition` sorts
    one tape in place and finds every match, handing back an owned `PartitionRun`;
    `emit_rows` drains that run into a `RowBuf` **up to a row limit**, keeping a
    `(match, row-within-match)` cursor plus the `BindIndex`/`AggMemo` so a batch
    boundary may fall inside a match. That last part is the memory bound: a single
    partition under ALL ROWS used to materialise its whole output first.
  - `rows` — `RowBuf`, the output rows row-major in one `Vec<Value>` with a column
    stride. A `Vec<Value>` per row cost a 24-byte header plus a `malloc` per row for
    a 32-byte-per-cell payload; on an 8M-row ALL ROWS result that was ~1 GB and 8M
    allocations.
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
  - `arrow_out.rs` — a `RowBuf` + output `Ty`s → a `RecordBatch`.
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
   and returns one finalize state per shard. **Matching is per partition; emitting is
   not.** Output rows come out in fixed-size batches with a cursor that can stop
   inside a match (`Plan::emit_rows`), so the result never scales with a partition —
   which matters because sharding cannot split a partition, so a single big one has
   no other defence.
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
- **DEFINE predicates are type-checked at bind** (`plan.rs::check_predicate`), not
  just parsed. They used to be, and the three failures that produced were all
  *silent empty results*: a non-boolean predicate (`{"B":"price"}`), a mistyped
  comparison (`{"B":"sym > 3"}` — three-valued logic makes every row not-true), and
  an unknown column, which raised `MrError::Eval` from inside the matcher and so
  only failed **if a row ever reached the predicate** — passing on a dev sample and
  failing in production. `Ty::Null` is accepted (a statically-NULL predicate is
  well-formed). Do not relax this to a runtime check; `tests/bind_diagnostics.rs`
  pins it, and `ordinary_predicates_still_bind` there is the guard against making
  `infer` stricter than `eval`.
- **There are two comparators, and they are pinned to each other.**
  `valops::compare` is SQL comparison (`None` = unordered, which callers turn into
  NULL); `cmp_for_sort`/`cmp_present` is the total tape order. They drifted once:
  the sort side was fixed to compare integers exactly and to use `total_cmp` for
  NaN, and `compare` was not — so `ORDER BY` and a DEFINE predicate disagreed about
  which rows were *equal*, which is a wrong match rather than a wrong value. Both
  now go through one `cmp_ints` (i128, exact across `BIGINT`/`UBIGINT`/`HUGEINT`),
  and `tests/compare.rs::comparators_agree_exactly` is the guard. They are allowed
  to differ on exactly two float ties, both documented there: NaN is unordered to
  SQL but must land somewhere in a total order, and `-0.0`/`0.0` are equal to SQL
  but distinct to `total_cmp`. `mr-worker/tests/sort_agreement.rs` pins the *sort*
  pair (`cmp_cells` vs `cmp_for_sort`) separately; a new key type needs an arm in
  both harnesses.
- **`Ty::UInt64` deliberately has no `numeric_rank`.** A rank means containment and
  `unify` takes the higher one, but `u64` and `i64` contain neither the other —
  sharing `Int64`'s rank makes `unify` non-commutative (`u + v` and `v + u` would
  type differently), and ranking either side of it re-creates the wrap. Its joins
  are explicit arms *before* the rank fallback, and falling through to `None` is
  the safety net: a forgotten pair is a bind error, not a silent mis-typing.
  Every `Ty` match fails closed that way; the `Value` matches do not, so the ones
  to watch when adding a variant are `value.rs`'s `as_f64`/`as_i128`/`as_bool` and
  `arrow_out`'s `to_i128`/`to_decimal`/`display` — six catch-alls that compile
  cleanly and return wrong data. `UInt8`/`16`/`32` stay `Int64` on purpose.
- **Overflow is checked in i128, because release builds do not check it at all.**
  `[profile.release]` sets no `overflow-checks` and CI sets no `RUSTFLAGS`, so
  `cargo test` catches a wrap and the shipped binary silently returns it — that is
  how `TIMESTAMP '9999-12-31' - epoch` shipped a *negative* interval. Temporal
  arithmetic, interval literals and decimal rescaling all compute wide and
  range-check once. When touching them, verify with
  `RUSTFLAGS="-C overflow-checks=on" cargo test --release --workspace`.
- **Errors from `define`/`measures` carry their key** via `MrError::with_context`,
  applied in `parse_define`/`parse_measures`. The expression parser is handed one
  string and cannot name it, so anything new added inside those per-key loops must
  go **inside** the wrapping closure or it loses the prefix. Measures are named by
  their `as`, not their index — that is what the output column is called.
- **Parse errors never print a Rust token name.** Both lexers have `Display for Tok`
  and a `lex_spanned` returning a character index per token; both parsers carry
  `src` + `spans` and raise through `err_at`/`err_here`, which append
  `diag::point_at` (source line + caret). `{tok:?}` in a user-facing message is a
  regression — `bind_diagnostics.rs` asserts no `RParen`/`Tok::`/`Some(` leaks.
  Positions are **char** indices, not bytes, since the caret is aligned by counting
  characters.
- **`Ty` has a `Display` giving the DuckDB spelling** (`BIGINT`, not `Int64`), and
  every message naming a type uses `{}`, not `{:?}` — the same reason as the tokens
  above: `Varchar` is not a word anyone can write in SQL. The spellings round-trip
  through `parse_type_name`, which is what makes a type quoted in an error
  pasteable into the `type` override; `every_rendered_type_name_parses_back` pins
  that. `Value` (runtime) and `AggKind` are still `{:?}` in a few eval-path
  messages — the same class, not yet done.
- **`vgi-lint` greps argument descriptions, so wording is load-bearing.** Two rules
  bite in `argument_specs()`: VGI313 fires when a description contains its own
  declared type name — which made the word "any" unusable in `define`/`subset` the
  moment those became type `any` ("avoids doubling **any** quotes" cost a point) —
  and VGI317 fires on phrasing that reads as enumerating allowed values, which
  "matches a row bound to **one of** its members" did. Both are substring
  heuristics, so reword rather than argue; re-run the lint after touching any
  description, since the metadata gate wants 100/100.
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
  grouping, and `pattern`/`define`/`measures` are user-supplied strings. The
  *compiler* has the third such cap, `MAX_PROGRAM_INSTS`: a bounded quantifier is
  expanded by copying its body, so the repeat counts multiply and `A{100000000}`
  or `((A{1000}){1000}){1000}` is an allocation failure — the process dies rather
  than the query. `emit_quant` rejects the count before looping over it, so an
  absurd bound fails promptly instead of spinning to the ceiling.
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
  runtime — one unused 200-byte column measured 2.8x on 2M rows. `include` columns
  are in that set: naming one is what makes it available at produce time.
- **`emit_rows` may stop mid-match, so its cursor is the whole correctness story.**
  `PartitionRun` carries `(mi, ri)` plus the `BindIndex` and `AggMemo` for the match
  in progress, and `loaded` says whether those two describe `matches[mi]` yet. The
  RUNNING horizon is `ri`, so a resumed row evaluates exactly as an uninterrupted one
  — but only because the index and the memo are *kept*, not rebuilt: rebuilding the
  memo mid-match would restart every running aggregate at zero. `tests/streaming_emit.rs`
  pins this by driving every limit from 1 upwards against `Plan::run`.
- **`sort_tape_on_keys` applies its permutation in place** by following cycles
  (`apply_permutation`), marking each slot with the `u32` permutation's top bit — so
  the key-sort path is capped at `MAX_ROWS_FOR_KEY_SORT` rows and anything larger
  falls back to `cmp_cells`. The cap is also the fix for a latent `n as u32` that
  wrapped silently. Copying the tape instead cost another 8 bytes per row of the
  partition, on the one shape (one huge partition) that has no other relief.
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
| `VGI_MR_FINALIZE_MEMORY_BYTES` | Spooled bytes above which the relation is sharded by partition key. Default 128 MB, at most 1024 shards. Halved from 256 MB once the split measured nearly free (see `shard.rs` for the table). Small values are how the sharded path gets exercised by hand; large ones are for a query whose spool outgrows the page cache, where the split's second pass does cost real time. |
| `VGI_MR_SPOOL_COMPRESSION` | `lz4` compresses every spooled record, `none` never does. Unset is size-triggered: a sink writes plain until it has written **half the finalize budget** (floor 32 MB) *uncompressed*, then switches, so a short query pays nothing — and, more importantly, a spool that will stay unsharded stays mappable, since a compressed record has to be inflated into the heap while an uncompressed one is borrowed from the mapping (measured 432 -> 314 MB peak RSS on 8M rows). Safe now that the shard count is derived from each record's uncompressed length rather than from file sizes — measuring bytes on disk let compression loosen the memory bound by its own ratio. |
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
