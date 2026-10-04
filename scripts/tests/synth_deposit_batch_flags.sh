#!/usr/bin/env bash
# scripts/tests/synth_deposit_batch_flags.sh — the script that starts a reth node for the synthetic deposit batches
# keeps the node off every public peer network: discovery off, no peers either way, p2p on loopback, no published
# devp2p port, RPC and Engine API on loopback only.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
S="$ROOT/scripts/synth-deposit-batch.sh"
FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

for flag in "--disable-discovery" "--max-outbound-peers 0" "--max-inbound-peers 0" "--addr 127.0.0.1" \
  "--http.addr 127.0.0.1" "--authrpc.addr 127.0.0.1"; do
  if grep -qF -- "$flag" "$S"; then pass "synth-deposit-batch.sh: $flag"; else fail "synth-deposit-batch.sh: $flag" "missing"; fi
done
if grep -qE -- '--(http|authrpc)\.addr 0\.0\.0\.0|--port|3030[34]' "$S"; then fail "no public bind or devp2p port" "found one"; else pass "no public bind or devp2p port"; fi

echo "== synth_deposit_batch_flags.sh :: $( [[ $FAILED -eq 0 ]] && echo ALL PASS || echo SOME FAILED ) =="
exit "$FAILED"
