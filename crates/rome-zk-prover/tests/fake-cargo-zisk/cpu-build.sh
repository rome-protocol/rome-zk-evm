#!/bin/sh
# Fake CPU-only build of cargo-zisk: `--version` carries the [cpu] tag. If anything asks it to prove, it leaves a
# `proved` marker next to itself, which the refusal test checks never appears.
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.2.0-alpha [cpu] (fbbc69b 2026-08-26T22:06:35.808557029Z)"
  exit 0
fi
touch "$(dirname "$0")/proved"
exit 1
