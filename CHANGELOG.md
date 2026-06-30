# Changelog

All notable changes to `vgi-matchrecognize` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/), and the project
adheres to [Semantic Versioning](https://semver.org/).

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
