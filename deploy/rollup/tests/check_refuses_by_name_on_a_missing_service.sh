#!/usr/bin/env bash
# deploy/rollup/tests/check_refuses_by_name_on_a_missing_service.sh — `rollup check` names the service that is not
# running and exits non-zero, for each of the four node services; the items that need that service are skipped with
# the reason rather than reported as a second, misleading failure; every other item still runs.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
prepare_check check_refuses_by_name_on_a_missing_service

for svc in sequencer batcher reth-verifier derive; do
  echo "$(tr ' ' '\n' < "$S/running" | grep -vx "$svc" | tr '\n' ' ')" > "$S/running"
  out="$("$ROLLUP" check 2>&1)"; code=$?
  [[ $code -ne 0 ]] && pass "check exits non-zero with $svc missing" || fail "check exits non-zero with $svc missing" "exit 0"
  grep -qE "^FAIL: service $svc — ServiceMissing: $svc is not running" <<<"$out" && pass "check names the missing service: $svc" || fail "check names the missing service: $svc" "$(grep -i service <<<"$out" | head -3)"
  others="$(grep -E '^FAIL: service ' <<<"$out" | grep -v "service $svc " || true)"
  [[ -z "$others" ]] && pass "$svc is the only service named as missing" || fail "$svc is the only service named as missing" "$others"
  set_world
done

# Missing derive: the item that needs it skips with the reason; unrelated items still run and pass.
echo "sequencer batcher reth-verifier" > "$S/running"
out="$("$ROLLUP" check 2>&1)"
grep -q '^SKIP: derive lag — .*derive' <<<"$out" && pass "derive lag is skipped with the reason when derive is down" || fail "derive lag is skipped with the reason" "$(grep 'derive lag' <<<"$out")"
grep -q '^PASS: chain id' <<<"$out" && pass "unrelated items still run (chain id)" || fail "unrelated items still run" "$out"
grep -q '^PASS: sequencer head' <<<"$out" && pass "unrelated items still run (sequencer head)" || fail "unrelated items still run" "$out"

# Nothing running at all: every service is named.
echo "" > "$S/running"
out="$("$ROLLUP" check 2>&1)"; code=$?
n="$(grep -cE '^FAIL: service .* — ServiceMissing' <<<"$out")"
[[ $code -ne 0 && $n -eq 4 ]] && pass "with nothing running, all four services are named" || fail "with nothing running, all four services are named" "exit=$code named=$n"

# Never reaches a cloud CLI or anything but docker and curl (the stubs record every call).
grep -qvE '^(docker|curl) ' "$S/calls" 2>/dev/null && fail "check only calls docker and curl" "$(grep -vE '^(docker|curl) ' "$S/calls" | head -2)" || pass "check only calls docker and curl"
finish check_refuses_by_name_on_a_missing_service
