#!/bin/sh
# Fake cargo-zisk that writes a partial `-o` file and then exits non-zero, as if the proving step
# failed after it had started writing — used to prove `LocalCargoZisk::prove` deletes that file on
# `ProveFailed`, not only on timeout.
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
printf 'garbage-partial-bytes-not-a-real-proof' > "$out"
echo "cargo-zisk: proving failed" >&2
exit 1
