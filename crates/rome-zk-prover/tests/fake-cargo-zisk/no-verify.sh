#!/bin/sh
# Fake cargo-zisk that exits 0 (as if proving finished) but never prints a verified line — the
# shape a real `--plonk` run that generated a proof but failed its own self-verify would produce.
set -eu
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
echo "Proof generated in 0.05s, steps: 10"
printf '\001fake-proof-bytes' > "$out"
exit 0
