#!/bin/sh
# Fake cargo-zisk that finishes NORMALLY (verified line, proof written, exit 0) but forks a helper
# that stays alive holding the stdout pipe — the shape of a tool whose sub-process outlives it.
# Exercises LocalCargoZisk on the fast-exit path: the call must still return promptly with the
# lines the tool printed, and the helper must not survive the call.
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
echo "<<< GENERATING_WRAPPER_SNARK_PROOF (77ms)"
echo "Proof generated in 0.077s, steps: 2048"
echo "SNARK proof was verified"
printf '\001fake-proof-bytes' > "$out"
sleep 3120 &
exit 0
