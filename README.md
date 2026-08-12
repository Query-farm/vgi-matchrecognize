<p align="center">
  <img src="docs/vgi-logo.png" alt="Vector Gateway Interface" width="360">
</p>

# vgi-matchrecognize

**SQL:2016 `MATCH_RECOGNIZE` row pattern matching for DuckDB.**

Oracle, Trino, Snowflake, Redshift, BigQuery and Flink all ship `MATCH_RECOGNIZE`.
DuckDB does not — so finding "three failed logins then a success", or "a price dip
followed by a recovery", means hand-rolling it out of `LAG`/`LEAD` chains,
gap-and-island tricks, recursive CTEs and correlated subqueries: slow to run,
miserable to read, and usually subtly wrong on ties and NULLs.

This is a [VGI](https://query.farm) worker that gives DuckDB the standard surface
as one table-in / table-out function. It buffers a relation, partitions it, sorts
each partition, runs a regular expression **over rows**, and emits either one
summary row per match or every matched row.

Local compute only: **no network, no secrets, no credentials.** The engine —
pattern compiler, Pratt expression parser, bind-time type inference, backtracking
matcher — is hand-rolled in the Arrow-free `mr-core` crate. (Input *is* spooled to
a temporary file while it buffers; see [Operating notes](#operating-notes).)

## Quick start

```sql
INSTALL vgi FROM community;
LOAD vgi;

-- The worker is a local binary. No secret needed: pure compute, no egress.
ATTACH 'mr' AS mr (TYPE vgi, LOCATION '/path/to/vgi-matchrecognize-worker');
```

Find each V-shaped dip — a falling run, then a rising run — per symbol. This runs
as-is:

```sql
SELECT *
FROM mr.match_recognize(
       (SELECT * FROM (VALUES
          ('ACME', 1, 10), ('ACME', 2,  8), ('ACME', 3,  6),
          ('ACME', 4,  9), ('ACME', 5, 11), ('ACME', 6,  7)
        ) AS t(symbol, ts, price)),
       partition_by := ['symbol'],
       order_by     := ['ts'],
       pattern      := 'START DOWN+ UP+',
       define       := '{
         "DOWN": "price < PREV(price)",
         "UP":   "price > PREV(price)"
       }',
       measures     := '{
         "match_no":  "MATCH_NUMBER()",
         "start_ts":  "FIRST(START.ts)",
         "bottom_ts": "LAST(DOWN.ts)",
         "end_ts":    "LAST(UP.ts)",
         "drawdown":  "FIRST(START.price) - LAST(DOWN.price)"
       }');
```

```text
┌─────────┬──────────┬──────────┬───────────┬────────┬──────────┐
│ symbol  │ match_no │ start_ts │ bottom_ts │ end_ts │ drawdown │
│ varchar │  int64   │  int64   │   int64   │ int64  │  int64   │
├─────────┼──────────┼──────────┼───────────┼────────┼──────────┤
│ ACME    │        1 │        1 │         3 │      5 │        4 │
└─────────┴──────────┴──────────┴───────────┴────────┴──────────┘
```

`START` has no predicate, so it matches any row; `DOWN+` then takes the falling
run and `UP+` the recovery. The dip at `ts = 6` starts no match because nothing
rises after it.

## The function

Everything except the relation is a named, bind-time constant. Functions live in
catalog `mr`, schema `main` — `mr.match_recognize(…)` and
`mr.main.match_recognize(…)` both resolve.

```text
mr.match_recognize(
    (<relation>),                -- positional: the input relation, buffered whole
    partition_by  := ['col', …], -- VARCHAR[]  (default [] = one global partition)
    order_by      := ['col', …], -- VARCHAR[]  (required; 'col DESC', 'col NULLS FIRST')
    pattern       := '…',        -- the row pattern: a regex over variables
    define        := '{…}',      -- JSON: { "VAR": "<boolean predicate>", … }
    subset        := '{…}',      -- JSON: { "U": ["A","B"], … }   (SQL:2016 SUBSET)
    measures      := '{…}',      -- JSON: { "out_col": "<expression>", … }
    rows          := 'one'|'all',-- ONE (default) | ALL ROWS PER MATCH
    empty_matches := 'show'|'omit', -- SHOW (default) | OMIT EMPTY MATCHES
    after         := '…',        -- AFTER MATCH SKIP mode (default 'past last row')
    step_budget   := 5000000     -- backtracking guard; omit to scale with partition size
) -> TABLE
```

Also: `mr.explain_pattern(p) -> VARCHAR` pretty-prints a compiled pattern (handy
for checking greediness, no data access), and `mr.main.after_match_skip_modes` is a
browsable list of the `after` modes. The worker's build version is the catalog's
`implementation_version` in `vgi_catalogs()`.

## Patterns

A regular expression over **pattern variables**, not characters:

| Construct     | Syntax                            | Meaning |
|---------------|-----------------------------------|---------|
| Variable      | `A`, `DOWN`, `START`              | one row satisfying `define["A"]` (any row if undefined) |
| Concatenation | `A B C`                           | A then B then C |
| Alternation   | `A \| B`                          | A or B — the left branch is preferred |
| Quantifiers   | `* + ? {n} {n,} {n,m} {,m} {,}`   | 0+, 1+, 0/1, exactly n, n+, n..m, 0..m, 0+ |
| Reluctant     | `A+?`, `A*?`, `A{2,}?`            | match as few rows as possible |
| Grouping      | `(A B)+`                          | quantify or alternate a sub-pattern |
| Anchors       | `^A`, `A$`                        | partition start / end |

Unquoted variable names are case-insensitive and canonicalize to upper case. A
**double-quoted** name is case-sensitive, so `"b"` and `b` are different variables
(the latter being `B`), and `CLASSIFIER()` reports the canonical spelling.

## DEFINE and MEASURES

Both clauses share one expression language. `define` decides whether a row can
bind a variable; `measures` computes the output columns.

| | you can write |
|---|---|
| Columns | `price`, `A.price`, `"My Col"` |
| Navigation | `PREV`/`NEXT`/`FIRST`/`LAST(expr[, n])` |
| Aggregates | `SUM` `COUNT` `AVG` `MIN` `MAX` `ARRAY_AGG` `ARBITRARY`, plus `COUNT(*)`, `COUNT()`, `COUNT(A.*)` |
| Match info | `CLASSIFIER([label])`, `MATCH_NUMBER()` |
| Horizon | `RUNNING` / `FINAL` |
| Operators | arithmetic, comparison, `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `BETWEEN`, `IN`, `\|\|`, `CAST`/`::` |
| Scalars | `abs` `ceil` `floor` `round` `sqrt` `lower` `upper` `trim` `ltrim` `rtrim` `length` `coalesce` `nullif` `greatest` `least` |

DEFINE predicates are always **RUNNING**: they see only the rows matched so far,
which is what lets a predicate refer back to the match in progress.

**A bare `A.price` means `LAST(A.price)`** under the prevailing RUNNING/FINAL
horizon — the last row bound to `A`, or NULL if `A` has not bound one yet. So
match-dependent predicates read the way you would write them:
`"B": "price > A.price"` compares each candidate `B` against `A`'s row. Qualified
physical navigation anchors on the variable too: `PREV(A.price, n)` steps back `n`
rows from `A`'s last row (the standard's `PREV(LAST(A.price), n)`), while
unqualified `PREV(price)` steps back from the current row.

**`array_agg(expr)`** collects matched values in match order as a DuckDB list
(`BIGINT[]`, `VARCHAR[]`, …). Over an empty match it is an empty list, not NULL —
so a RUNNING `array_agg` grows row by row.

**`subset := '{"U": ["A","B"]}'`** declares SQL:2016 union variables. `U` then
stands for any of its members wherever a pattern variable may appear: `U.price`,
`COUNT(U.*)`, `SUM(U.price)`, `CLASSIFIER(U)`, `after := 'to last U'`. A union
variable may not have its own DEFINE predicate.

### Anything else: compose around the call

The expression language is a deliberate subset, and it does not need to be
complete — DuckDB's full library is available on both sides of the call:

```sql
-- row-local scalar: compute it in the input subquery
(SELECT *, lower(event) AS ev FROM t)   →  define := '{"A": "ev = ''view''"}'

-- post-processing a measure: do it in the outer SELECT
SELECT CAST(lower(cls) || '_label' AS VARCHAR(7))
FROM mr.match_recognize(…, measures := '{"cls": "LAST(CLASSIFIER())"}')
```

What composition cannot reach: an **unsupported scalar inside a DEFINE predicate**
(the predicate feeds back into matching, so it has to run in the matcher), an
**unsupported aggregate over match state** (it depends on which rows are bound, so
neither side of the call can see it), and **subqueries** in either clause — a
standalone worker receives Arrow batches and has no catalog to resolve them
against.

## What comes out

The output schema is fixed at bind time, before any data flows.

- **`rows := 'one'`** — the `partition_by` columns, then one column per measure.
- **`rows := 'all'`** — the `partition_by` columns, the `order_by` columns,
  `match_number BIGINT` and `classifier VARCHAR` (automatic unless a measure of
  that name shadows them), then one column per measure.

> **Deviation from Trino/Oracle.** Under `ALL ROWS PER MATCH` they emit *every*
> input column and no automatic `match_number`/`classifier`. Project any other
> input column through a measure — `{"value": "value"}` — if you need it.

Measure **types are inferred** from the input schema: `MATCH_NUMBER()` and
`COUNT(…)` → `BIGINT`; `CLASSIFIER()` → `VARCHAR`;
`FIRST`/`LAST`/`PREV`/`NEXT`/`MIN`/`MAX` → that column's type; `SUM` widens
(integer → `HUGEINT`, float → `DOUBLE`); `AVG` → `DOUBLE`; `ARRAY_AGG` → a list of
the argument's type; arithmetic → the widened numeric type; comparison and logic →
`BOOLEAN`; `||` → `VARCHAR`. When inference cannot decide — a measure that resolves
to an untyped `NULL`, say — use the array form and pin the type:

```json
[
  { "as": "ratio", "expr": "SUM(A.qty) / SUM(B.qty)", "type": "DECIMAL(18,6)" },
  { "as": "label", "expr": "CLASSIFIER()" }
]
```

## Semantics worth knowing

**`MATCH_NUMBER()` counts within a partition.** It restarts at 1 for every
partition, as SQL:2016 specifies — it is not a global row number.

**Empty matches are real matches.** A pattern that legitimately matches zero rows
at some position — `B*` where the row cannot bind `B` — produces one output row
positioned on the row it sits on, and consumes a match number. Its measures see a
match with nothing bound: `CLASSIFIER()` and the navigation functions are NULL,
`COUNT(*)` is 0, `array_agg` is empty. This surprises people, and it is what every
conforming engine does. `empty_matches := 'omit'` drops those rows under
`rows := 'all'`; `rows := 'one'` always reports them.

**AFTER MATCH SKIP** decides where the next search begins:

| `after`               | Next search resumes at        | notes |
|-----------------------|-------------------------------|-------|
| `'past last row'`     | the row after the match       | non-overlapping (default) |
| `'to next row'`       | the row after the match start | overlapping matches |
| `'to first <VAR>'`    | the first row bound to VAR    | VAR may be a union variable |
| `'to last <VAR>'`     | the last row bound to VAR     | VAR may be a union variable |

A no-progress safeguard advances the cursor by at least one row per iteration, so
the scan always terminates regardless of the skip mode. `mr.main.after_match_skip_modes`
lists the same thing from SQL.

## Recipes

### Sessionization — split a stream where the gap exceeds 30 minutes

```sql
SELECT user_id, session_no, session_start, session_end, n_events
FROM mr.match_recognize(
       (SELECT user_id, ts FROM clicks),
       partition_by := ['user_id'],
       order_by     := ['ts'],
       pattern      := 'A B*',
       define       := '{ "B": "ts <= PREV(ts) + INTERVAL 1800 SECOND" }',
       measures     := '{
         "session_no":    "MATCH_NUMBER()",
         "session_start": "FIRST(ts)",
         "session_end":   "LAST(ts)",
         "n_events":      "COUNT(*)"
       }');
```

`A` takes any row; `B*` extends the session for as long as each event is within
30 minutes of the previous one. The next session starts at the first row that is
not.

### Brute force then breach — three or more failures, then a success

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
         "var":        "CLASSIFIER()",
         "n_fails":    "FINAL COUNT(FAIL.*)",
         "first_fail": "FIRST(FAIL.ts)",
         "breach_ts":  "LAST(OK.ts)"
       }',
       rows := 'all');   -- every event in the burst, tagged by classifier
```

### Funnel — view → click → purchase, in order, per user

```sql
SELECT *
FROM mr.match_recognize(
       (SELECT user_id, ts, event FROM events),
       partition_by := ['user_id'],
       order_by     := ['ts'],
       pattern      := 'V+ C+ P',
       define       := '{
         "V": "event = ''view''",
         "C": "event = ''click''",
         "P": "event = ''purchase''"
       }',
       measures     := '{
         "views":    "COUNT(V.*)",
         "clicks":   "COUNT(C.*)",
         "elapsed":  "LAST(P.ts) - FIRST(V.ts)",
         "sequence": "array_agg(CLASSIFIER())"
       }');
```

## Operating notes

### Buffering and memory

Row pattern matching is intrinsically a whole-partition operation, so the function
buffers its input before it matches anything. Batches are spooled to the worker's
cross-process store as Arrow IPC — by default a SQLite file under `$TMPDIR`, not
process memory.

- **Unused columns are free.** Only the columns the pattern reads — the partition
  and order keys plus everything `define`/`measures` reference — are buffered; the
  rest are projected away first. This matters more than it sounds: buffering
  volume dominates runtime, and on 2M rows a single unused 200-byte column cost
  1.02s → **2.83s** before this was added.
- **Output streams** one partition at a time, coalesced into ~8k-row batches, so
  the whole result set is never live at once.
- **Input does not stream.** At finalize the buffered relation is read back into
  memory, plus one `usize` per row for the partition tapes. Measured: 5M rows × 4
  columns → about **0.6 GB** peak worker RSS producing 1.33M matches. Filter or
  aggregate before the function on inputs far larger than RAM; per-partition
  spilling is the next step.

Matching cannot be made fully streaming: a match may span an entire partition, so
the partition must be complete first, and DuckDB does not deliver input clustered
by partition key.

Leave `VGI_WORKER_SHARED_STORAGE` alone: the buffering and producing phases may run
in *different worker processes*, so the store has to outlive a process, and the
in-process `memory` backend is rejected at bind time for exactly that reason. Every
batch carries an independent row count that is checked when the relation is read
back, so a short read is an error rather than a quietly incomplete answer.

### Not hanging, not crashing

An ambiguous pattern can backtrack catastrophically, so the matcher runs on a
**per-partition step budget** and returns a clean error rather than running forever.
It never panics or aborts: property tests drive arbitrary patterns over arbitrary
row tables, including partitions of 12,000–20,000 rows.

The budget **scales with the partition** (128 steps per row, floor 5,000,000),
because a constant is the wrong shape — catastrophic backtracking is super-linear in
partition size, so one number cannot both catch a bad pattern on a small partition
and let an ordinary linear match finish on a large one. Pin it with
`step_budget := <n>` for a hard ceiling.

Backtracking uses an explicit heap stack rather than host recursion, so one match
may span millions of rows: an `A B*` sessionization whose partition is a single long
session is bounded by the budget, not by the OS stack. Deeply nested
`pattern`/`define`/`measures` input is refused past 128 levels for the same reason.

## Conformance

**132 assertions ported from Trino's `MATCH_RECOGNIZE` test suite** run as part of
the end-to-end suite (`test/sql/trino_conformance.test`). Of the 150 cases
expressible on this surface, **none produces a wrong answer**: the remaining 18
error cleanly on features we do not implement — subqueries in `DEFINE`/`MEASURES`,
two-argument aggregates (`max_by`), a few scalar functions, and the `ALL ROWS`
column layout above.

Still unimplemented: `PERMUTE`, pattern exclusion `{- … -}`, `WITH UNMATCHED ROWS`,
and some exotic temporal/`INTERVAL` type-lattice corners (route those through the
explicit `type` override).

[`test/trino/README.md`](test/trino/README.md) has the full tally, the bugs the
port found, and how to regenerate it.

## Build & test

```sh
cargo test --workspace     # mr-core unit + property tests, mr-worker integration
cargo build --release      # the worker binary
./run_tests.sh             # haybarn SQLLogic end-to-end suite
./test/trino/port.sh       # re-port Trino's suite (needs a Trino checkout)
```

## License

[MIT](LICENSE) — Copyright 2026 Query Farm LLC.
