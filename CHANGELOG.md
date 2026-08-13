# Changelog

All notable changes to `vgi-matchrecognize` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/), and the project
adheres to [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-08-13

### Added

- **A container image**, `ghcr.io/query-farm/vgi-matchrecognize`, multi-arch and
  cosign-signed, built by the shared `vgi-actions/docker-publish.yml` workflow —
  the server/Fly.io path alongside the release archives that DuckDB spawns
  on-host. It serves HTTP (`/health` on `$PORT`, the default entrypoint), raw
  Arrow-IPC over TCP (`$PORT_TCP`), and stdio.

  Two operational notes are specific to this worker being a *buffering* function
  rather than a stateless one, and both are verified in CI against the signed
  `vgi` extension:

  - It spools its input to `$TMPDIR`, so a container needs temp space of roughly
    24 bytes per row of the columns the pattern reads (about 1.5x that while a
    sharded run splits). No volume is declared, so by default that lands in the
    container's writable layer.
  - Every phase of a query must reach the same spool. Over HTTP or TCP one
    container serves all of them; over **stdio the extension spawns a pool of
    workers, so each spawn is a separate container** and they must share a volume
    on `/tmp` (`docker run -i --rm -v mr-spool:/tmp IMG stdio`). Without it the
    sink-count guard raises rather than returning a short result — which is what
    it is for.

### Changed

- The README carries status shields, a links section (downloads, image, changelog,
  performance baseline, and the `MATCH_RECOGNIZE` documentation of the engines the
  conformance suites are ported from), and a section on running the container.

## [0.2.0] - 2026-08-13

### Changed

- **Output rows stream in fixed-size batches, including through a long match.**
  The producer used to materialize one whole partition's output before it could
  emit anything, and `Vec<Vec<Value>>` cost a header plus an allocation per row.
  Under `rows := 'all'` a partition emits one row per input row, so a relation
  that is one big partition — exactly the shape sharding cannot split — scaled
  its peak memory with the result: **8M rows measured 2.70 GB, now 0.63 GB**
  (2M: 648 → 153 MB; 4M: 1326 → 258 MB), and it got faster too (3.00 → 2.16 s).
  Emission now runs off a `(match, row-within-match)` cursor that can stop inside
  a match and resume with the same RUNNING horizon, bind index and aggregate
  accumulators, into a row-major `RowBuf`.
- **The finalize memory budget defaults to 128 MB**, down from 256 MB. The split
  pass is much cheaper than when that number was chosen — on 8M rows x 3 BIGINT
  it costs ~5% of wall clock and halves peak RSS (362 → 216 MB), and on 40M rows
  it is within 3% of not splitting at all while holding memory to 634 MB against
  1774 MB. `VGI_MR_FINALIZE_MEMORY_BYTES` still overrides it; raise it for a
  query whose spool outgrows the page cache, where the second pass does cost
  real time.
- **Spool compression starts at half the finalize budget** (floor 32 MB) instead
  of a fixed 32 MB per sink. Compression is not free in memory: an uncompressed
  record is decoded straight out of the mapping and its arrays borrow it, while a
  compressed one is inflated into the heap and stays there for the producer's
  life. The fixed threshold meant a query whose spool fit the budget — one that
  was going to be read back whole — turned most of the relation into anonymous
  memory to save disk it was not short of. Measured on 8M rows: 432 → 314 MB.
- **Partition tapes are built as one contiguous buffer** (rows + per-partition
  bounds + labels) by a counting pass, instead of a growable `Vec` per partition.
  That removes a header and an allocation per partition and up to 8 bytes per row
  of doubling slack that was never returned.
- **The sort applies its permutation in place**, following cycles rather than
  copying the tape, which saves another 8 bytes per row of the largest partition.

- **DEFINE predicates are type-checked at bind time.** They were only parsed, so
  a predicate that could never be true produced an empty result and no message.
  All three of these now fail at bind, naming the key:
  `define := {"B": "price"}` (not boolean), `{"B": "sym > 3"}` (VARCHAR compared
  with INTEGER), and `{"B": "prcie < PREV(price)"}` (unknown column). The last
  was the worst of them — an unknown column raised an *evaluation* error from
  inside the matcher, so whether the query failed depended on whether any row
  reached the predicate: it passed on a small sample and failed in production.
  A statically-NULL predicate is still accepted, being well-formed SQL.
- **Errors out of `define` / `measures` name the key they came from.** The
  expression parser sees one string and cannot say which of a dozen measures it
  was handed, so `plan` re-attaches the context: `measures['y']: unknown
  function 'lsat'`. The array form names a measure by its `as`, not its index.
- **Parse errors quote source text and point at it.** `expected RParen` named
  our AST rather than the user's pattern; both parsers now render the token as
  written and append the source with a caret:

  ```text
  match_recognize pattern error: expected ')', found end of pattern
      A (B | C D
                ^
  ```
- **Types are named in SQL.** `Ty` gained a `Display` giving the DuckDB spelling,
  so an inference error reads `cannot compare VARCHAR with BIGINT` rather than
  `cannot compare Varchar with Int64`. Every rendered name round-trips through
  `parse_type_name`, so a type quoted back at you can be pasted straight into a
  measure's `type` override.

### Added

- **`include := ['col', …]`** carries input columns through to the output
  unchanged, next to the partition keys — the value on each matched row under
  `rows := 'all'`, on the match's first row under `'one'`. SQL:2016 ALL ROWS PER
  MATCH passes the whole input row through; this function emits only what the
  query reads, because buffering a column it never reads is the most expensive
  thing it can do (one unused 200-byte column measured 2.8x), so passthrough is
  opt-in per column. A column already emitted as a partition or order key is not
  repeated, and an unknown one fails at bind.

- **`UBIGINT` is a first-class type.** It used to be folded into `BIGINT` and
  read with `as i64`, so any value above `i64::MAX` arrived negative and stayed
  that way through comparison, sorting, partitioning and output —
  `18446744073709551615` came back as `-1`. It now round-trips as `UBIGINT`.
  Because `u64` and `i64` contain neither the other, mixing them widens to
  `HUGEINT` (including `-u`); `SUM` → `HUGEINT` and `AVG` → `DOUBLE` as for any
  integer. `UTINYINT`..`UINTEGER` still map to `BIGINT`, which is exact for them.
  Known limit: an integer *literal* is lexed as `BIGINT`, so a constant above
  `i64::MAX` must be written `CAST('18446744073709551615' AS UBIGINT)`.

### Fixed

- **`NOT` bound tighter than comparison.** `NOT x IS NULL` parsed as
  `(NOT x) IS NULL`, and since `NOT NULL` is NULL that is true exactly when `x`
  IS NULL — the inverse of what was written, with no error anywhere. Now that
  DEFINE is type-checked at bind, the arithmetic forms had also become hard
  failures: `NOT price > 10` parsed as `(NOT price) > 10` and was rejected as
  non-boolean. SQL orders these OR < AND < NOT < comparison, and so do we.
  `NOT BETWEEN` / `NOT IN` / `IS NOT NULL` are unaffected.
- **Integers were compared as `f64`, and NaN compared equal to everything.**
  `valops::compare` — behind every DEFINE predicate, `IN`, `BETWEEN`, `MIN`/`MAX`,
  `NULLIF` and `GREATEST`/`LEAST` — sent both numeric families through `as_f64`.
  Past 2^53 that collapses adjacent `BIGINT`s onto one float, so
  `9007199254740993 = 9007199254740992` was TRUE and
  `1700000000000000001 > 1700000000000000000` was FALSE; nanosecond epochs and
  snowflake ids sit squarely in that range. The sort comparator had already been
  fixed for both hazards, so `ORDER BY` and a DEFINE predicate disagreed about
  which rows were equal — a wrong *match*, not merely a wrong value. Both now
  share one exact i128 comparison, and a property test pins them together.
  Separately, `NaN = 1.0` was TRUE and `NaN <> 1.0` FALSE; both are NULL now.
- **`TIMESTAMP - TIMESTAMP` overflowed past 2262.** Rescaling i64 ticks to
  nanoseconds leaves i64 well inside the timestamp range, so
  `TIMESTAMP '9999-12-31' - epoch` panicked under `cargo test` and returned a
  silently *negative* interval in release, where nothing enables overflow
  checks. All temporal arithmetic now computes in i128; a difference too large
  for the nanosecond field carries whole days instead, which is lossless.
  `INTERVAL` literals (`INTERVAL 9223372036854775807 HOUR`) and `DATE ± INTERVAL`
  had the same defect.
- **`DATE - DATE` and `TIME - TIME` could never evaluate.** Both type-check as
  `INTERVAL` at bind but had no evaluation arm, so they failed at produce time
  with `non-numeric operand Date(...)`.
- **A bounded quantifier could exhaust memory.** Its body is expanded by
  copying, so the repeat counts multiply: `A{100000000}` and
  `((A{1000}){1000}){1000}` were allocation failures that killed the worker
  rather than errors that failed the query. The compiled program is now capped,
  and an absurd bound is rejected before the expansion starts.
- A negative `DECIMAL` scale — which Arrow permits — panicked in `CAST` and in
  `ceil`/`floor`/`round`; `rescale` wrapped instead of saturating; and a NULL
  element in a `VARCHAR[]` argument was an error at one string width and a
  phantom empty name at the other.
- The catalog's `vgi.doc_llm` claimed "nothing on disk". The worker spools the
  buffered relation to the system temp directory; the text now says so, since
  anyone reading it through `vgi_catalogs()` may be doing a data-residency
  review.

## [0.1.0] - 2026-06-30

Initial release: SQL:2016 `MATCH_RECOGNIZE` row pattern matching for DuckDB.

### Added

- **`mr.main.match_recognize((<relation>), partition_by:=, order_by:=,
  pattern:=, define:=, measures:=, rows:=, after:=, [step_budget:=])`** — a
  table-in / table-out buffering function that partitions and sorts the input
  relation and runs a backtracking row-pattern matcher over it.
  - **PATTERN**: concatenation, alternation `|`, quantifiers `* + ? {n} {n,}
    {n,m} {,m}` (greedy and reluctant `?`), grouping `()`, and partition-edge
    anchors `^ $`.
  - **DEFINE / MEASURES expression language**: column refs, variable-qualified
    refs (`A.price`), literals, `PREV`/`NEXT`/`FIRST`/`LAST(expr[,n])`, running
    aggregates `SUM`/`COUNT`/`AVG`/`MIN`/`MAX`, `CLASSIFIER()`,
    `MATCH_NUMBER()`, `RUNNING`/`FINAL`, arithmetic / comparison / `AND` `OR`
    `NOT` / `IS [NOT] NULL` / `BETWEEN` / `IN` / `||`, and `CAST` / `::`.
  - **Bind-time MEASURES type inference** from the input Arrow schema, with an
    explicit `{"as":…,"expr":…,"type":…}` override escape hatch.
  - **`rows := 'one' | 'all'`** and all four **AFTER MATCH SKIP** modes
    (`past last row`, `to next row`, `to first <VAR>`, `to last <VAR>`).
  - A per-partition **step budget** (default 5,000,000) that aborts
    catastrophic backtracking cleanly — the matcher never hangs and never
    panics.
- **`mr.main.mr_version()`** — the worker version string.
- **`mr.main.explain_pattern(p)`** — pretty-print a compiled pattern (no data).

### Deferred to v1.1 (documented non-goals)

- `PERMUTE`, `SUBSET`, and pattern exclusion `{- … -}`.
- `WITH UNMATCHED ROWS` / `SHOW EMPTY MATCHES`.
- Exotic temporal / `INTERVAL` type-lattice corners (route through the explicit
  `type` override).
