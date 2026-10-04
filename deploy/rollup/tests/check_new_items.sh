#!/usr/bin/env bash
# deploy/rollup/tests/check_new_items.sh — the `rollup check` items that read Solana through rome-zk-ops or judge a window:
# batches finalized against blocks sealed (a stuck batcher fails by name), the verification key, the payer floor, the
# reclaim deadline and the exit configuration. Each has a passing and a failing case; the failing ones name what is wrong.
# docker (rome-zk-ops) and curl are stubs; every answer comes from a file under $S.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
prepare_check check_new_items

run() { "$ROLLUP" check 2>&1; }
expect() { # $1=output $2=pattern $3=label
  grep -qE -- "$2" <<<"$1" && pass "$3" || fail "$3" "no line /$2/ in: $(grep -E '^(PASS|FAIL|SKIP|WARN)' <<<"$1" | head -20 | tr '\n' '|')"
}
absent() { # $1=output $2=pattern $3=label
  grep -qE -- "$2" <<<"$1" && fail "$3" "unexpected line /$2/ in: $1" || pass "$3"
}
status_file() { printf '%s\n' "$@" > "$S/chain_status"; }

# ---- batches finalized against blocks sealed -------------------------------------------------------------------------
# blocks_per_batch 30 and the close age in the fixture give a window of 900 s. CHECK_NOW sets the clock.
batches() { rm -f "$ROLLUP_OUT/check-samples"; echo 700 > "$S/sealed_total"; echo 40 > "$S/finalized_total"; }
batches
out="$(CHECK_NOW=1000 run)"
expect "$out" '^SKIP: batches finalized — no sample from at least 900s ago' "the first run only records a sample and says so"
[[ -s "$ROLLUP_OUT/check-samples" ]] && pass "the first run wrote the samples file" || fail "the first run wrote the samples file" "no $ROLLUP_OUT/check-samples"

echo 800 > "$S/sealed_total"
out="$(CHECK_NOW=1500 run)"
expect "$out" '^SKIP: batches finalized — no sample from at least 900s ago' "a sample younger than the window is not judged yet"

echo 800 > "$S/sealed_total"; echo 40 > "$S/finalized_total"
out="$(CHECK_NOW=2000 run)"; code=$?
expect "$out" '^FAIL: batches finalized — BatchesNotFinalizing: 100 blocks sealed in the last 1000s' "blocks sealed and no batch finalized in the window fails by name"
[[ $code -ne 0 ]] && pass "a stuck batcher makes check exit non-zero" || fail "a stuck batcher makes check exit non-zero" "exit $code"
absent "$out" 'oldest_unposted' "the check no longer reads the batcher's age gauge"

batches; CHECK_NOW=1000 run >/dev/null
echo 800 > "$S/sealed_total"; echo 41 > "$S/finalized_total"
out="$(CHECK_NOW=2000 run)"
expect "$out" '^PASS: batches finalized — sealed \+100 blocks, finalized \+1 batches in the last 1000s' "blocks sealed with a batch finalized passes"

batches; CHECK_NOW=1000 run >/dev/null
echo 700 > "$S/sealed_total"; echo 40 > "$S/finalized_total"
out="$(CHECK_NOW=2000 run)"
expect "$out" '^PASS: batches finalized — sealed \+0 blocks' "an idle chain (no blocks sealed) is not a stuck batcher"

batches; CHECK_NOW=1000 run >/dev/null
echo 20 > "$S/sealed_total"; echo 0 > "$S/finalized_total"   # both processes restarted: their counters start again
out="$(CHECK_NOW=2000 run)"
absent "$out" '^FAIL: batches finalized' "a restart that resets the counters is not read as a stuck batcher"

batches; CHECK_NOW=1000 run >/dev/null
echo 800 > "$S/sealed_total"
out="$(CHECK_NOW=2000 CHECK_FINALIZE_WINDOW_SECS=1200 run)"
expect "$out" '^SKIP: batches finalized .*1200s ago' "CHECK_FINALIZE_WINDOW_SECS moves the window"

echo "sequencer reth-verifier derive" > "$S/running"; out="$(CHECK_NOW=2000 run)"
expect "$out" '^SKIP: batches finalized — needs the sequencer and the batcher' "without the batcher the item is skipped with the reason"
echo "sequencer batcher reth-verifier derive" > "$S/running"
batches

# ---- the verification key (only with a prover) ------------------------------------------------------------------------
out="$(run)"
expect "$out" '^SKIP: verification key — no prover configured' "the verification key item is skipped without a prover"
echo "PROVER=on" >> "$ROLLUP_ENV"; echo "sequencer batcher reth-verifier derive prover postgres" > "$S/running"
out="$(run)"
expect "$out" '^PASS: verification key — active for the proving layout \(1 registry entries\)' "an active verification key passes"
status_file chain_id=4295391538 slot=1000 reclaim_slots_left=none vkey_entries=1 vkey_active=no vkey_pending_activation_slot=5000
out="$(run)"
expect "$out" '^FAIL: verification key — VkeyNotActive: .*activates at slot 5000, now at slot 1000' "a key that is registered but not yet active fails by name, with the slot"
status_file chain_id=4295391538 slot=1000 reclaim_slots_left=none vkey_entries=0 vkey_active=no
out="$(run)"
expect "$out" '^FAIL: verification key — VkeyNotActive: no verification key is active' "no key registered at all fails by name"
touch "$S/chain_status_fail"; out="$(run)"
expect "$out" '^FAIL: verification key — ChainStatusUnreadable: RootLookupFailed' "an unreadable registry is a named failure, never a pass"
rm "$S/chain_status_fail"
echo "sequencer batcher reth-verifier derive" > "$S/running"
grep -v '^PROVER=on$' "$ROLLUP_ENV" > "$ROLLUP_ENV.new" && mv "$ROLLUP_ENV.new" "$ROLLUP_ENV"
rm -f "$S/chain_status"

# ---- the payer floor --------------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^PASS: payer balance — 5000000000 lamports at .* \(floor 1000000000\)' "a payer above the default floor (1 SOL) passes"
echo 999999999 > "$S/payer_lamports"; out="$(run)"
expect "$out" '^FAIL: payer balance — PayerBelowFloor: 999999999 lamports at .*floor 1000000000' "a payer below the floor fails by name with both numbers"
expect "$out" 'PAYER_FLOOR_LAMPORTS' "the failure says how to move the floor"
echo 5000000000 > "$S/payer_lamports"
out="$(PAYER_FLOOR_LAMPORTS=6000000000 run)"
expect "$out" '^FAIL: payer balance — PayerBelowFloor: 5000000000 lamports .*floor 6000000000' "PAYER_FLOOR_LAMPORTS in the environment moves the floor"
echo "PAYER_FLOOR_LAMPORTS=2000000000" >> "$ROLLUP_ENV"; out="$(run)"
expect "$out" '^PASS: payer balance — .*floor 2000000000' "PAYER_FLOOR_LAMPORTS in .env moves the floor"
grep -v '^PAYER_FLOOR_LAMPORTS=' "$ROLLUP_ENV" > "$ROLLUP_ENV.new" && mv "$ROLLUP_ENV.new" "$ROLLUP_ENV"
echo missing > "$S/payer_lamports"; out="$(run)"
expect "$out" '^FAIL: payer balance — PayerBalanceUnreadable' "an RPC that does not answer is a named failure"
echo 5000000000 > "$S/payer_lamports"
mv "$ROLLUP_OUT/chain-id.env" "$ROLLUP_OUT/chain-id.env.off"; out="$(run)"
expect "$out" '^SKIP: payer balance — needs .*chain-id.env' "without the chain id record the payer item is skipped with the reason"
mv "$ROLLUP_OUT/chain-id.env.off" "$ROLLUP_OUT/chain-id.env"

# ---- the reclaim deadline ---------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^PASS: reclaim deadline — the chain has posted a root' "a chain that has posted a root cannot be reclaimed"
status_file chain_id=4295391538 slot=1000 reclaim_slots_left=900000 vkey_entries=1 vkey_active=yes
out="$(run)"; code=$?
expect "$out" '^PASS: reclaim deadline — 900000 slots left \(about 100 hours\), margin 108000' "a deadline far away passes and says how far"
status_file chain_id=4295391538 slot=1000 reclaim_slots_left=50000 vkey_entries=1 vkey_active=yes
out="$(run)"; code=$?
expect "$out" '^FAIL: reclaim deadline — ReclaimDeadlineNear: 50000 slots left' "a deadline inside the margin fails by name"
[[ $code -ne 0 ]] && pass "a near deadline fails the check, so a timer acts before the chain can be reclaimed" || fail "a near deadline fails the check" "exit $code"
out="$(RECLAIM_MARGIN_SLOTS=40000 run)"
expect "$out" '^PASS: reclaim deadline — 50000 slots left .*margin 40000' "RECLAIM_MARGIN_SLOTS in the environment moves the margin"
status_file chain_id=4295391538 slot=1000 reclaim_slots_left=0 vkey_entries=1 vkey_active=yes
out="$(run)"; code=$?
expect "$out" '^FAIL: reclaim deadline — ReclaimDeadlinePassed' "a passed deadline fails by name"
[[ $code -ne 0 ]] && pass "a passed deadline makes check exit non-zero" || fail "a passed deadline makes check exit non-zero" "exit $code"
status_file chain_id=4295391538 slot=1000 vkey_entries=1 vkey_active=yes
out="$(run)"
expect "$out" '^FAIL: reclaim deadline — ChainStatusUnreadable: no reclaim_slots_left line' "chain-status output without the line is a named failure"
touch "$S/chain_status_fail"; out="$(run)"
expect "$out" '^FAIL: reclaim deadline — ChainStatusUnreadable: RootLookupFailed' "an unreadable chain is a named failure, not a pass"
rm "$S/chain_status_fail"; rm -f "$S/chain_status"

# ---- the exit config, shown --------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^PASS: exit config — .*exit_portal \(current\) 0x4200000000000000000000000000000000000016' "the exit portal is shown"
expect "$out" '^PASS: exit config — .*pending_mask 0 \(none\); activation_slot n/a' "the pending change is shown"
touch "$S/exit_config_fail"; out="$(run)"
expect "$out" '^FAIL: exit config — ExitConfigUnreadable: ExitConfigLookupFailed' "an unreadable exit config is a named failure"
rm "$S/exit_config_fail"

# ---- the image is needed for the three that read Solana through rome-zk-ops -------------------------------------------
cp "$ROLLUP_OUT/compose.env" "$ROLLUP_OUT/compose.env.keep"
grep -v '^ROME_ZK_TAG=\|^ROME_ZK_IMAGE=' "$ROLLUP_OUT/compose.env.keep" > "$ROLLUP_OUT/compose.env"
for k in ROME_ZK_TAG ROME_ZK_IMAGE; do [[ -z "${!k:-}" ]] || unset "$k"; done
grep -v '^ROME_ZK_TAG=\|^ROME_ZK_IMAGE=' "$ROLLUP_ENV" > "$ROLLUP_ENV.new" && mv "$ROLLUP_ENV.new" "$ROLLUP_ENV"
out="$(run)"
expect "$out" '^FAIL: reclaim deadline — ChainStatusUnreadable: ImageTagNotSet' "no image: the reclaim deadline names ImageTagNotSet"
expect "$out" '^FAIL: exit config — ExitConfigUnreadable: ImageTagNotSet' "no image: the exit config names ImageTagNotSet"
mv "$ROLLUP_OUT/compose.env.keep" "$ROLLUP_OUT/compose.env"
finish check_new_items
