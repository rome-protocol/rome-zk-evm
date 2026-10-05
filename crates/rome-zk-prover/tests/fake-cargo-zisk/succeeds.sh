#!/bin/sh
# Fake cargo-zisk for rome-zk-prover's LocalCargoZisk tests: emits the same log lines the real
# `cargo-zisk prove --plonk` writes, then writes a stand-in proof file at the `-o` path and
# exits 0. Never invokes any real ZisK toolchain.
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
echo "<<< GENERATING_WRAPPER_SNARK_PROOF (11238ms)"
echo "Proof generated in 0.123s, steps: 4096"
echo "SNARK proof was verified"
printf '\001fake-proof-bytes' > "$out"
exit 0
