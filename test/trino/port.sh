#!/usr/bin/env bash
# Re-port Trino's row-pattern test suite and regenerate
# test/sql/trino_conformance.test.
#
# Needs a Trino checkout (TRINO_HOME, default ~/Development/trino) and a release
# worker build. The pipeline is:
#
#   extract.py    Trino JUnit sources        -> cases.json   (query, expected)
#   translate.py  native MATCH_RECOGNIZE     -> t.json       (our function call)
#   run_conformance.py  run + diff vs Trino  -> r.json       (PASS/FAIL/ERROR)
#   emit_suite.py       passing cases        -> the .test file
#
# Cases that cannot be translated (SUBSET, PERMUTE, exclusion syntax, ...) and
# cases that still error (unsupported scalar/aggregate functions, subqueries) are
# reported but left out of the generated suite; see README.md for the tally.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TRINO_HOME="${TRINO_HOME:-$HOME/Development/trino}"
SRC="$TRINO_HOME/core/trino-main/src/test/java/io/trino/sql/query"
WORK="${WORK:-$(mktemp -d)}"

if [[ ! -d "$SRC" ]]; then
    echo "ERROR: Trino sources not found at $SRC" >&2
    echo "       clone Trino and/or set TRINO_HOME" >&2
    exit 1
fi

cd "$(dirname "${BASH_SOURCE[0]}")"
echo "==> Building the worker (release)"
cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" --bin vgi-matchrecognize-worker

echo "==> Extracting cases from Trino"
python3 extract.py \
    "$SRC/TestRowPatternMatching.java" \
    "$SRC/TestAggregationsInRowPatternMatching.java" > "$WORK/cases.json"

echo "==> Translating to mr.main.match_recognize(...)"
python3 translate.py "$WORK/cases.json" "$WORK/t.json"

echo "==> Running against the worker"
python3 run_conformance.py "$WORK/t.json" "$WORK/r.json"

echo "==> Regenerating test/sql/trino_conformance.test"
python3 emit_suite.py "$WORK/r.json" "$REPO_ROOT/test/sql/trino_conformance.test"

echo "==> Work directory: $WORK"
echo "    inspect a case with:  python3 show.py <testMethodName> [PASS|FAIL|ERROR]"
echo "    (show.py/diagnose.py read r.json from the current directory)"
