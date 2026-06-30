#!/usr/bin/env bash
# Build the matchrecognize VGI worker and run the SQLLogic tests against it using
# the haybarn DuckDB distribution's unittest runner (which ships the `vgi`
# extension via the community repository).
#
# Prerequisites (one-time):
#   uv tool install haybarn-unittest
#   uv tool install haybarn
#   echo "INSTALL vgi FROM community;" | uvx haybarn-cli
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

UNITTEST="${VGI_UNITTEST:-$(command -v haybarn-unittest || true)}"
if [[ -z "$UNITTEST" || ! -x "$UNITTEST" ]]; then
    echo "ERROR: haybarn-unittest not found. Install it with:" >&2
    echo "       uv tool install haybarn-unittest" >&2
    exit 1
fi

if ! echo "LOAD vgi;" | uvx haybarn-cli >/dev/null 2>&1; then
    echo "==> Installing vgi extension from community repository"
    echo "INSTALL vgi FROM community;" | uvx haybarn-cli
fi

echo "==> Building vgi-matchrecognize-worker (release)"
cargo build --release --bin vgi-matchrecognize-worker

WORKER="$REPO_ROOT/target/release/vgi-matchrecognize-worker"
TEST_GLOB="${1:-test/sql/*}"

echo "==> Running SQLLogic tests"
echo "    worker:   $WORKER"
echo "    unittest: $UNITTEST"
echo "    tests:    $TEST_GLOB"

VGI_MATCHRECOGNIZE_WORKER="$WORKER" \
VGI_WORKER_CATALOG_NAME="mr" \
    "$UNITTEST" --test-dir "$REPO_ROOT" "$TEST_GLOB"
