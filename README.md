# vgi-matchrecognize

**SQL:2016 `MATCH_RECOGNIZE` row pattern matching for DuckDB.**

DuckDB has **no native `MATCH_RECOGNIZE`**. Oracle, Trino, Snowflake, Redshift,
BigQuery, and Flink all ship it; on DuckDB you otherwise hand-roll the pattern
with fragile `LAG`/`LEAD` chains, gap-and-island window tricks, recursive CTEs,
and correlated subqueries that are slow, unreadable, and wrong on ties and
nulls. `vgi-matchrecognize` is a [VGI](https://query.farm) worker that brings the
**standard row-pattern surface** to DuckDB as a single table-in / table-out
function: buffer a relation, partition it, sort each partition, run a
regular-expression-over-rows matcher, and emit either one summary row per match
or every matched row.

Pure local compute: **no network, no secrets, nothing on disk.** The whole
engine — pattern compiler, Pratt expression parser, bind-time type inference,
and backtracking matcher — is hand-rolled in the Arrow-free `mr-core` crate.

## Install & attach

```sql
INSTALL vgi FROM community;
LOAD vgi;

-- The worker is a local binary; no secret needed (pure compute, no egress).
ATTACH 'mr' AS mr (TYPE vgi, COMMAND 'vgi-matchrecognize-worker');
```

## The function

```text
mr.match_recognize(
    (<relation>),               -- positional: the input relation, buffered whole
    partition_by := ['col', …], -- VARCHAR[]   (default [] = one global partition)
    order_by     := ['col', …], -- VARCHAR[]   (required; 'col DESC' / 'NULLS FIRST' ok)
    pattern      := '…',        -- the row pattern (regex over variables)
    define       := '{…}',      -- JSON: { "VAR": "<boolean predicate>", … }
    measures     := '{…}',      -- JSON object/array of output expressions
    rows         := 'one'|'all',-- ONE ROW PER MATCH (default) | ALL ROWS PER MATCH
    after        := '…',        -- AFTER MATCH SKIP mode (default 'past last row')
    step_budget  := 5000000     -- per-partition backtracking guard (optional)
) -> TABLE
```

Plus `mr.explain_pattern(p) -> VARCHAR` (pretty-print a compiled pattern; no
data access) and the `mr.main.after_match_skip_modes` reference view. The worker
build version is published as the catalog's `implementation_version` (read it
from `vgi_catalogs()`), not a scalar function.

## Examples

### V-shape stock dip — a falling run then a rising run

```sql
SELECT *
FROM mr.match_recognize(
       (SELECT symbol, ts, price FROM ticks),
       partition_by := ['symbol'],
       order_by     := ['ts'],
       pattern      := 'START DOWN+ UP+',
       define       := '{
         "DOWN": "price < PREV(price)",
         "UP":   "price > PREV(price)"
       }',
       measures     := '{
         "match_no":     "MATCH_NUMBER()",
         "start_ts":     "FIRST(START.ts)",
         "bottom_ts":    "LAST(DOWN.ts)",
         "bottom_price": "LAST(DOWN.price)",
         "end_ts":       "LAST(UP.ts)",
         "drawdown":     "FIRST(START.price) - LAST(DOWN.price)"
       }',
       rows  := 'one',           -- ONE ROW PER MATCH (default)
       after := 'past last row'  -- AFTER MATCH SKIP PAST LAST ROW (default)
     );
-- one row per V per symbol
```

### Brute-force then breach — ≥3 failed logins immediately followed by a success

```sql
SELECT *
FROM mr.match_recognize(
       (SELECT user_id, ts, outcome FROM auth_events),
       partition_by := ['user_id'],
       order_by     := ['ts'],
       pattern      := 'FAIL{3,} OK',
       define       := '{
         "FAIL": "outcome = ''fail''",
         "OK":   "outcome = ''success''"
       }',
       measures     := '{
         "match_no":   "MATCH_NUMBER()",
         "var":        "CLASSIFIER()",
         "n_fails":    "FINAL COUNT(FAIL.*)",
         "first_fail": "FIRST(FAIL.ts)",
         "breach_ts":  "LAST(OK.ts)"
       }',
       rows  := 'all',           -- ALL ROWS PER MATCH: each event, tagged by classifier
       after := 'past last row'
     );
```

### Sessionization — split a stream where the inter-event gap exceeds 30 minutes

```sql
SELECT user_id, session_no, session_start, session_end, n_events
FROM mr.match_recognize(
       (SELECT user_id, ts, event FROM clicks),
       partition_by := ['user_id'],
       order_by     := ['ts'],
       pattern      := 'A B*',
       define       := '{ "B": "ts <= PREV(ts) + INTERVAL 1800 SECOND" }',
       measures     := '{
         "session_no":    "MATCH_NUMBER()",
         "session_start": "FIRST(ts)",
         "session_end":   "LAST(ts)",
         "n_events":      "COUNT(*)"
       }',
       rows := 'one'
     );
```

## Pattern grammar

A regular expression **over pattern variables** (not characters):

| Construct        | Syntax                              | Meaning |
|------------------|-------------------------------------|---------|
| Variable         | `A`, `DOWN`, `START`                | one row satisfying `DEFINE[A]` (or always-true if undefined) |
| Concatenation    | `A B C`                             | A then B then C |
| Alternation      | `A \| B`                            | A or B (left preferred under greedy) |
| Quantifiers      | `* + ? {n} {n,} {n,m} {,m}`         | 0+, 1+, 0/1, exactly n, n+, n..m, 0..m |
| Reluctant        | `A+?`, `A*?`, `A{2,}?`              | match as few as possible |
| Grouping         | `(A B)+`                            | quantify/alternate a sub-pattern |
| Anchors          | `^A`, `A$`                          | partition start / end |

## DEFINE / MEASURES expression language

Column refs (`price`), variable-qualified refs (`A.price`), literals,
`PREV`/`NEXT`/`FIRST`/`LAST(expr[,n])`, running aggregates
`SUM`/`COUNT`/`AVG`/`MIN`/`MAX` (incl. `COUNT(*)` and `COUNT(A.*)`),
`CLASSIFIER()`, `MATCH_NUMBER()`, `RUNNING`/`FINAL`, arithmetic, comparison,
`AND`/`OR`/`NOT`, `IS [NOT] NULL`, `BETWEEN`, `IN`, `||`, and `CAST`/`::`.
DEFINE predicates are always **RUNNING** (they see only rows matched so far).

## Output schema (fixed at bind time)

- **`rows := 'one'`** — the `partition_by` columns, then one column per measure.
- **`rows := 'all'`** — the `partition_by` columns, the `order_by` columns,
  `match_number BIGINT`, `classifier VARCHAR` (auto, unless a measure shadows
  them), then one column per measure.

Measure **types are inferred** from the input Arrow schema at bind time:
`MATCH_NUMBER()`/`COUNT(...)` → `BIGINT`, `CLASSIFIER()` → `VARCHAR`,
`FIRST`/`LAST`/`PREV`/`NEXT`/`MIN`/`MAX`/aggregate-of-column → that column's
type, `SUM` widens (integer → `HUGEINT`, float → `DOUBLE`), `AVG` → `DOUBLE`,
arithmetic → the widened numeric type, comparison/logical → `BOOLEAN`, `||` →
`VARCHAR`. When inference can't decide, supply the **array form** with an
explicit override:

```json
[
  { "as": "ratio", "expr": "SUM(A.qty) / SUM(B.qty)", "type": "DECIMAL(18,6)" },
  { "as": "label", "expr": "CLASSIFIER()" }
]
```

## AFTER MATCH SKIP

| `after` value          | Next search resumes at | Notes |
|------------------------|------------------------|-------|
| `'past last row'`      | row after the match    | non-overlapping (default) |
| `'to next row'`        | row after match start  | overlapping matches |
| `'to first <VAR>'`     | first row bound to VAR | — |
| `'to last <VAR>'`      | last row bound to VAR  | — |

A no-progress safeguard forces the cursor forward by one row each iteration, so
matching always terminates independently of the step budget.

## Robustness

The matcher backtracks with a **per-partition step budget** (default 5,000,000):
on a pathological, ambiguous pattern it returns a clean error rather than
hanging, and it never panics (enforced by a property test over arbitrary
patterns × arbitrary row tables). Buffer-all-then-compute holds the whole input
relation in memory — block or filter before the function on very large inputs;
partitioning keeps each working set small.

## Scope

**v1 (this release):** one `match_recognize` table function +
`explain_pattern`; the pattern grammar, expression language, inference, `rows`,
and AFTER MATCH SKIP above.

**Deferred to v1.1:** `PERMUTE`, `SUBSET`, pattern exclusion `{- … -}`, `WITH
UNMATCHED ROWS` / `SHOW EMPTY MATCHES`, and exotic temporal/`INTERVAL`
type-lattice corners (route those through the explicit `type` override).

## Build & test

```sh
cargo test --workspace          # mr-core unit + proptest, mr-worker integration
cargo build --release           # build the worker binary
./run_tests.sh                  # haybarn SQLLogic end-to-end suite
```

## License

[MIT](LICENSE) — Copyright 2026 Query Farm LLC.
