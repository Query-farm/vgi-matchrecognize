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

### Fixed

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
