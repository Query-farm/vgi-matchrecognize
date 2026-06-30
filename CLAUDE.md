# CLAUDE.md

Guidance for working in this repository.

## What this is

`vgi-matchrecognize` is a **VGI worker** (a standalone binary DuckDB launches and
talks to over Apache Arrow IPC, `ATTACH 'mr' (TYPE vgi, COMMAND '…')`) that
brings **SQL:2016 `MATCH_RECOGNIZE` row pattern matching** to DuckDB, which has
no native support for it. Functions live under catalog `mr`, schema `main`.

Built on the published VGI Rust SDK (`vgi = "0.9.5"` from crates.io), arrow 59.
The repo builds standalone — no local SDK checkout, no path deps except the
intra-workspace `mr-core`. License **MIT**.

## SQL surface

- `mr.main.match_recognize((<relation>), partition_by:=, order_by:=, pattern:=,
  define:=, measures:=, rows:=, after:=, [step_budget:=])` — the one real
  function: a **table-in / table-out buffering function**. The relation is a
  subquery (NOT a correlated `LATERAL`); everything else is a scalar `const_arg`
  (named). `partition_by`/`order_by` are `VARCHAR[]`; `pattern`/`rows`/`after`
  are `VARCHAR`; `define`/`measures` are JSON strings.
- `mr.main.mr_version()` — the worker version (fleet convention).
- `mr.main.explain_pattern(p)` — pretty-print a compiled pattern; no data.

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
- **`crates/mr-worker`** — thin Arrow/VGI adapter:
  - `match_recognize.rs` — the `TableBufferingFunction` (`on_bind` / `process` /
    `combine` / `finalize_producer`); buffers each batch into `storage`, then
    concatenates, builds the `Plan`, runs it, and emits one output batch.
  - `arrow_in.rs` — a `RowStore` over a concatenated `RecordBatch`.
  - `arrow_out.rs` — `Vec<Vec<Value>>` + output `Ty`s → a `RecordBatch`.
  - `schema.rs` — `Ty` ↔ Arrow `DataType` + the `ArrowBindSchema` for inference.
  - `scalar/` — `mr_version`, `explain_pattern`.
  - `catalog.rs` / `meta.rs` — catalog/schema/function metadata for `vgi-lint`.
  - `main.rs` registers everything and calls `Worker::run()`.

## The two hard design points

1. **Buffer-all, then compute.** Row pattern matching is intrinsically a
   whole-partition operation, so `match_recognize` is a `TableBufferingFunction`
   (Sink+Source), the `vgi-match` idiom: `process` buffers each Arrow batch into
   cross-process `storage`; `finalize_producer` concatenates everything, runs the
   matcher per partition, and streams the result back.
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
- The matcher is a backtracking VM: it leaves `binds` unchanged on a failed
  branch (the `Char` instruction push/pops; control instructions never touch
  bindings), so backtracking is just "return `None`". Termination is guaranteed
  two ways: the per-partition **step budget** (no hang) bounds inner work, and
  the outer match loop advances the tape cursor by ≥1 row each iteration.
- Empty matches (zero bound rows) are omitted from output and don't consume a
  match number.
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

Metadata gate: `uvx --from vgi-lint-check@0.37.0 vgi-lint lint
"$PWD/target/release/vgi-matchrecognize-worker" --fail-on info` → 100/100.
