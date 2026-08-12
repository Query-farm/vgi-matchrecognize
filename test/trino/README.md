# Trino row-pattern conformance port

Trino has the most thorough public `MATCH_RECOGNIZE` test suite available, so we
port it and run it against this worker. `test/sql/trino_conformance.test` is the
**generated** result: every case in it is a Trino assertion that we reproduce
exactly.

Regenerate with `./port.sh` (needs a Trino checkout — `TRINO_HOME`, default
`~/Development/trino`).

## Pipeline

| step | script | what it does |
|------|--------|--------------|
| 1 | `extract.py` | Pulls `(query, expected)` pairs out of the JUnit sources. Flattens Java to a placeholder string first (string literals → `\x01n\x01`), so balanced-paren scanning is exact; handles `+` concatenation, text blocks (`"""…"""`), and `format(query, …)` templates resolved against the *most recent preceding* `String query =` (several methods declare their own). |
| 2 | `translate.py` | Rewrites native `MATCH_RECOGNIZE (…)` into `mr.main.match_recognize(…)`: clause splitting, `MEASURES e AS n` → JSON, `DEFINE n AS e` → JSON, `AFTER MATCH SKIP …` → our `after` string, `OMIT EMPTY MATCHES` → `empty_matches := 'omit'`. Raises `Unsupported` for constructs we do not implement. |
| 3 | `run_conformance.py` | Runs each case and compares to Trino's expected `VALUES` with a symmetric `EXCEPT ALL`. Normalizes Trino literal syntax DuckDB rejects (`VARCHAR 'x'`, `array(varchar)`, bare `VALUES 1, 2`). |
| 4 | `emit_suite.py` | Writes the passing cases out as SQLLogic. |

`diagnose.py` re-runs the erroring cases one at a time to capture their messages;
`show.py <testMethod> [PASS|FAIL|ERROR]` prints a case side by side with Trino's
expected result. Both read `r.json` from the working directory that `port.sh`
prints.

## Results

Sources: `TestRowPatternMatching.java`, `TestAggregationsInRowPatternMatching.java`.
(`TestRowPatternMatchingInWindow.java` is excluded — window pattern recognition is
a different SQL surface that this worker does not implement.)

```
174 assertions extracted
135 translated to our surface
103 PASS      <- checked in as test/sql/trino_conformance.test
  0 FAIL      <- no wrong answers: everything unsupported errors cleanly
 32 ERROR     <- unsupported features, see below
```

Fully green Trino test methods: `testRowPattern`, `testPatternQuantifiers` (32
cases), `testEmptyCycle` (12), `testOutputModes` (8), `testNavigationFunctions`
(18 of 23), `testAfterMatchSkip`, `testPartitioning`, `testPartitioningAndOrdering`,
`testBackReference`, `testRunningAndFinal`, `testExponentialMatch`,
`testTentativeLabelMatch`, `testBalancingSums`, `testLabelAndColumnNames`.

### Bugs this port found and fixed

- **Empty matches were dropped.** A pattern matching zero rows at a position
  (`B*` where the row cannot bind `B`) is a real match in SQL:2016: it reports a
  row and consumes a match number. We omitted them entirely, which was the single
  largest source of wrong answers.
- **`MATCH_NUMBER()` did not reset per partition.** It counts matches *within* a
  partition; we threaded one counter across all of them.
- **Nested navigation dropped the outer offset.** `PREV(LAST(x), n)` must anchor
  on the row `LAST(x)` designates and then step back `n` physical rows; the
  logical navigation re-derived its own row, so `PREV` was silently a no-op.
- **Quantified empty sub-patterns were rejected.** `()*`, `^+`, `(){5,}`,
  `(B ()*)*` are legal and terminate: repetition of a sub-pattern that always
  matches zero rows is idempotent, so it collapses to a bounded form.
- **`{,}`** (equivalent to `*`) was a parse error.

### Not implemented (the 32 errors + 39 untranslatable)

Untranslatable (39): `SUBSET` union variables (17), pattern exclusion `{- … -}`
(10), omitted `ORDER BY` (4, we require it), `WITH UNMATCHED ROWS` (2), `PERMUTE`
(1), plus 5 cases whose shape the translator does not handle (two
`MATCH_RECOGNIZE` in one query, a non-`SELECT` head, `DEFINE` without `AS`).

Errors (32), by cause:

- **~18 — scalar and aggregate functions outside our expression language**:
  `abs`, `lower`, `coalesce`, `array_agg`, `max_by`, `arbitrary`, and
  parameterized CAST targets (`varchar(7)`). Our DEFINE/MEASURES language is a
  documented subset — navigation, the five aggregates, `CLASSIFIER`,
  `MATCH_NUMBER`, arithmetic/comparison/logic, `CAST`.
- **5 — subqueries in `DEFINE`/`MEASURES`** (`(SELECT …)`, `EXISTS`,
  `IN (SELECT …)`). Architecturally out of reach: the worker is a standalone
  process that receives Arrow batches and has no SQL engine to evaluate a
  correlated subquery against.
- **5 — `SUBSET` names referenced from `DEFINE`/`MEASURES`** (the rest of the
  `SUBSET` cases; deferred with the feature).
- **3 — `ALL ROWS PER MATCH` output layout.** Trino emits *every* input column and
  no automatic `match_number`/`classifier`; we emit the partition and order
  columns plus automatic `match_number`/`classifier`. A deliberate difference, not
  a bug — see the README's "Output schema" section.
- **1 — case-sensitive quoted labels** (`PATTERN ("Label")`). We canonicalize
  variable names to upper case.
