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
    `combine` / `finalize_producer`); buffers each batch into `storage`, then
    builds the `Plan` and returns a `PartitionStream` producer that matches one
    partition per step, coalescing output into ~8k-row batches.
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
   (Sink+Source), the `vgi-match` idiom: `process` buffers each Arrow batch into
   cross-process `storage` (on disk as Arrow IPC — the SDK's default backend is a
   SQLite file under `$TMPDIR`); `finalize_producer` reads it back and streams the
   result, running the matcher **one partition at a time**. The partition is the
   smallest sound streaming unit: a match may span a whole partition, so a
   partition must be complete before it can be matched.
2. **Output schema is fixed at `on_bind`** — before any data flows. The measure
   types are **inferred statically** from `params.input_schema` + the parsed
   measure ASTs (`mr-core::types::infer`), with an explicit `{"as","expr","type"}`
   override escape hatch for anything inference can't decide.

## Conventions / gotchas

- All algorithms live in `mr-core` with unit + property tests; the worker is a
  thin adapter. The pure core is testable with `VecRowStore` — no IPC, no DuckDB.
- Logs go to **stderr** — stdout is the Arrow-IPC channel.
- The catalog name must match the ATTACH name; `main.rs` defaults
  `VGI_WORKER_CATALOG_NAME` to `mr`.
- `serde_json` is built with `preserve_order` so a MEASURES **object**'s key
  order is the output column order (the SDK contract).
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
- `mr-worker/src/buffer.rs` owns the scan-cursor convention. Backends promise only
  *monotonic* ids: SQLite starts at 1, the fs store at 0, so paging from
  `after_id = 0` silently dropped the first batch. Start below every id.
  `tests/storage_probe.rs` round-trips every backend through the real helper.
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

## Build & test

```sh
cargo test --workspace                       # unit + proptest + worker integration
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
`VGI_MATCHRECOGNIZE_WORKER` pointed at the binary.

Metadata gate: `uvx --from vgi-lint-check vgi-lint lint
"$PWD/target/release/vgi-matchrecognize-worker" --fail-on info` → 100/100.
