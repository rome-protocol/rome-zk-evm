#!/bin/sh
# Fake cargo-zisk that writes a partial (garbage, mid-write) `-o` file, then hangs past any sane test
# timeout — the exact shape a real proving run killed mid-write leaves behind. Used to
# prove `LocalCargoZisk::prove` deletes that partial file on `ProveTimeout` rather than leaving it for
# a resume gate to mistake for a real proof.
set -eu
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf 'garbage-partial-bytes-not-a-real-proof' > "$out"
sleep 5
exit 0
