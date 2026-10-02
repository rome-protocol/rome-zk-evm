#!/usr/bin/env bash
# scripts/check-platform-tools.sh — refuse BY NAME if the installed `cargo build-sbf` platform-tools bundle is
# older than the minimum SBPF v3 needs (platform-tools v1.54 supports `cargo build-sbf --arch v3`). Run this
# BEFORE any `--arch v3` build so a stale installer fails fast with a clear reason instead of a
# confusing downstream build error.
#
# Usage: check-platform-tools.sh [min-version, default 1.54]
set -euo pipefail

MIN_VERSION="${1:-1.54}"

if ! command -v cargo-build-sbf >/dev/null 2>&1; then
  echo "PlatformToolsMissing: cargo-build-sbf not on PATH" >&2
  exit 1
fi

version_output="$(cargo-build-sbf --version 2>&1 || true)"
# `cargo build-sbf --version` prints a `platform-tools vX.Y[.Z]` line among its output; pull the first
# such version token out rather than assuming a fixed line number/format (this has drifted across Agave
# releases before).
found="$(echo "$version_output" | grep -oE 'platform-tools v[0-9]+\.[0-9]+(\.[0-9]+)?' | head -1 | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)"

if [[ -z "$found" ]]; then
  echo "PlatformToolsVersionUnknown: could not parse a platform-tools version out of:" >&2
  echo "$version_output" >&2
  exit 1
fi

# Compare MIN_VERSION vs found using sort -V (version-aware sort); found is new enough iff it is NOT
# the smaller of the two in a version sort (i.e. MIN_VERSION sorts first or equal).
smallest="$(printf '%s\n%s\n' "$MIN_VERSION" "$found" | sort -V | head -1)"
if [[ "$smallest" != "$MIN_VERSION" ]]; then
  echo "PlatformToolsTooOld: found platform-tools v$found, need >= v$MIN_VERSION for --arch v3" >&2
  exit 1
fi

echo "check-platform-tools: platform-tools v$found >= v$MIN_VERSION OK"
