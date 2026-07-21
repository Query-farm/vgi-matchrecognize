#!/usr/bin/env bash
# Copyright 2026 Query Farm LLC - https://query.farm
#
# Verify the workspace package version matches a GitHub Release / git tag, so the
# version the worker advertises over VGI (the catalog `implementation_version`)
# and the uploaded release binaries all equal the release. Run on a version-tag
# push before building/publishing anything.
#
# Usage: ci/check-version.sh <release-tag>      # e.g. v0.1.0 or 0.1.0
set -euo pipefail

TAG="${1:?usage: check-version.sh <release-tag>}"
TAG="${TAG#v}"  # accept an optional leading 'v'

HERE="$(cd "$(dirname "$0")" && pwd)"
CARGO_TOML="$HERE/../Cargo.toml"

VERSION="$(awk '
  /^\[workspace\.package\]/ { in_wp = 1; next }
  /^\[/                     { in_wp = 0 }
  in_wp && /^[[:space:]]*version[[:space:]]*=/ {
    if (match($0, /"[^"]+"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
  }
' "$CARGO_TOML")"

if [ -z "$VERSION" ]; then
  echo "::error::could not read [workspace.package] version from $CARGO_TOML" >&2
  exit 1
fi

if [ "$TAG" != "$VERSION" ]; then
  echo "::error::release tag ($TAG) does not match workspace version ($VERSION); bump [workspace.package] version in Cargo.toml before tagging." >&2
  exit 1
fi

echo "version OK: $VERSION matches release tag"
