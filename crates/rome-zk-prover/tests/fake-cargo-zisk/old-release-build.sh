#!/bin/sh
# Fake GPU build of an OLDER ZisK release than the one the vkey of record names: `--version` says 1.2.0-alpha. If
# anything asks it to prove, it leaves a `proved` marker next to itself, which the refusal test checks never appears.
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.2.0-alpha [gpu] (fbbc69b fake-build)"
  exit 0
fi
touch "$(dirname "$0")/proved"
exit 1
