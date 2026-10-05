#!/bin/sh
# Fake cargo-zisk that exits 0 and prints a verified line (as if proving finished and
# self-verified) but never actually writes the `-o` output file — exercises LocalCargoZisk's own
# post-verified check that the proof file actually landed and is non-empty.
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
echo "Proof generated in 0.05s, steps: 10"
echo "SNARK proof was verified"
exit 0
