#!/bin/sh
# Fake CPU-only build of cargo-zisk: `--version` carries the [cpu] tag. If anything asks it to prove, it leaves a
# `proved` marker next to itself, which the refusal test checks never appears.
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
touch "$(dirname "$0")/proved"
exit 1
