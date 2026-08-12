<p align="center">
  <img src="docs/vgi-logo.png" alt="Vector Gateway Interface" width="360">
</p>

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
ATTACH 'mr' AS mr (TYPE vgi, LOCATION '/path/to/vgi-matchrecognize-worker');
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
    step_budget  := 5000000     -- per-partition backtracking guard (optional;
                                --   omit to scale it with the partition size)
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

A bare `A.price` is the standard's shorthand for `LAST(A.price)` under the
prevailing RUNNING/FINAL semantics — the last row bound to `A`, or NULL if `A`
has not bound one yet. So match-dependent predicates work as written:
`"B": "price > A.price"` compares each candidate `B` row against `A`'s row.
Qualified physical navigation anchors on the variable too: `PREV(A.price, n)`
steps back `n` rows from `A`'s last row (the standard's `PREV(LAST(A.price), n)`),
while unqualified `PREV(price)` steps from the current row.

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

The matcher backtracks with a **per-partition step budget**: on a pathological,
ambiguous pattern it returns a clean error rather than hanging, and it never panics
or aborts (enforced by property tests over arbitrary patterns × arbitrary row
tables, including partitions of 12,000–20,000 rows).

The budget **scales with the partition** by default (128 steps per row, floor
5,000,000). A fixed constant is the wrong shape here: catastrophic backtracking is
super-linear in the partition size, so a constant both lets a pathological pattern
run a long time on a small partition and cuts off an ordinary linear match on a
large one. Pass `step_budget := <n>` to pin it instead.

Backtracking runs on an **explicit heap stack**, not host recursion, so a single
match may span millions of rows — a `A B*` sessionization where one partition is
one long session, or `A+` over a whole partition, is bounded by the step budget
rather than by the OS stack. Deeply nested `pattern` / `define` / `measures` input
is refused past 128 levels of nesting, again to keep a pathological string from
exhausting the stack.

## Memory & streaming

Input is **buffered to disk, not held in RAM**: each incoming batch is written to
the worker's cross-process store as Arrow IPC (a SQLite file under `$TMPDIR` by
default; set `VGI_WORKER_SHARED_STORAGE=memory|fs|sqlite` to choose a backend).

At finalize the buffered relation is read back and matched. Two properties keep
the footprint down:

- The batches are **not concatenated** — `BatchRowStore` addresses them as one
  contiguous row space, so there is no second full copy of the input.
- Output is **streamed one partition at a time**, coalesced into ~8k-row batches,
  so only the current batch's rows are materialized rather than the whole result.

What still scales with input size is the relation itself (read back into memory at
finalize) plus one `usize` per row for the partition tapes. Measured: 5M rows × 4
columns → **552 MB** peak worker RSS producing 1.33M matches. Matching cannot be
made fully streaming — a match may span an entire partition, so a partition has to
be complete before it can be matched, and DuckDB does not deliver input clustered
by partition key. Per-partition spilling is the next step for inputs larger than
RAM; until then, block or filter before the function on very large inputs.

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
