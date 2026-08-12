# Changelog

All notable changes to `vgi-matchrecognize` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/), and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

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
