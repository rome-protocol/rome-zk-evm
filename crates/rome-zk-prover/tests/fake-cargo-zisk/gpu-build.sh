#!/bin/sh
# Fake GPU build of cargo-zisk: `--version` carries the [gpu] tag; `prove` records its arguments next to itself
# (args.txt) and then behaves like succeeds.sh. Never invokes any real ZisK toolchain.
set -eu
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [gpu] (306a9c9 fake-build)"
  exit 0
fi
echo "$*" > "$(dirname "$0")/args.txt"
echo "${ZISK_HOME:-unset}" > "$(dirname "$0")/zisk_home.txt"
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
