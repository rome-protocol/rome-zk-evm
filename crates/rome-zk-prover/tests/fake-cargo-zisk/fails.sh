#!/bin/sh
# Fake cargo-zisk that exits non-zero, as if the STARK/PLONK proving step itself failed.
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
echo "cargo-zisk: proving failed" >&2
exit 1
