#!/usr/bin/env bash
# deploy/rollup/tests/register_confirm.sh — `rollup register --confirm` against a stubbed docker (the rome-zk-ops run) and curl. A permissionless
# registration sends NO verifier keys (the program refuses them), re-reads the payer's nonce first and stops by name
# (NonceAdvanced) when the id init derived is no longer the one the program would derive, before anything is sent.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
ROME_ZK_TAG=test-tag "$ROLLUP" init >/dev/null 2>&1
make_register_stubs
printf 'ab%.0s' $(seq 32) > "$WORK/keys/sequencer.key"

: > "$WORK/ops_calls"
if out="$("$ROLLUP" register --confirm 2>&1)"; then pass "register --confirm succeeds with no VKEY_JSON and no ZISK_VKEY_JSON set"; else fail "register --confirm succeeds with no verifier-key settings" "$out"; fi
[[ "$(sends)" == 1 ]] && pass "registration was sent once" || fail "registration was sent once" "$(cat "$WORK/ops_calls")"
if grep -qi 'vkey' "$WORK/ops_calls"; then fail "no rome-zk-ops call carries a verifier-key flag" "$(grep -i vkey "$WORK/ops_calls")"; else pass "no rome-zk-ops call carries a verifier-key flag"; fi
grep '^register --keypair' "$WORK/ops_calls" | grep -q -- '--permissionless' && pass "the registration is permissionless" || fail "the registration is permissionless" "$(cat "$WORK/ops_calls")"
reg_line="$(grep '^register --keypair' "$WORK/ops_calls" | head -1)"
[[ "$reg_line" == *' --nonce 0 '* && "$reg_line" == *' --expect-chain-id 4295391538'* ]] && pass "the send carries init's recorded nonce and chain id (the program refuses a stale pair)" || fail "the send carries --nonce and --expect-chain-id from the record" "$reg_line"
[[ "$(grep -c '^chain-id ' "$WORK/ops_calls")" == 2 ]] && pass "register read the payer and nonce twice: once for the authority check, once again right before sending" || fail "register re-reads the nonce" "$(cat "$WORK/ops_calls")"
first_idx="$(grep -n '^chain-id ' "$WORK/ops_calls" | head -1 | cut -d: -f1)"; send_idx="$(grep -n '^register --keypair' "$WORK/ops_calls" | head -1 | cut -d: -f1)"
[[ "$first_idx" -lt "$send_idx" ]] && pass "the nonce was re-read before the send" || fail "the nonce is re-read before the send" "$(cat "$WORK/ops_calls")"
grep -q '^CURSOR_PDA=StubCursorPda$' "$ROLLUP_OUT/pdas.env" && grep -q '^ROOT_PDA=StubRootPda$' "$ROLLUP_OUT/pdas.env" && pass "pdas.env written" || fail "pdas.env written" "$(cat "$ROLLUP_OUT/pdas.env" 2>/dev/null)"
rm -f "$ROLLUP_OUT/pdas.env"

# The payer's nonce moved after init: refuse by name, send nothing.
: > "$WORK/ops_calls"
if out="$(STUB_NONCE=1 "$ROLLUP" register --confirm 2>&1)"; then fail "register refuses when the nonce advanced" "exited 0"
elif grep -q '^NonceAdvanced' <<<"$out"; then pass "register refuses by name (NonceAdvanced) when the nonce moved since init"; else fail "register refuses by name (NonceAdvanced)" "$out"; fi
grep -q 'chain id would be 4295391539' <<<"$out" && pass "the refusal names the id the program would now derive" || fail "the refusal names the new id" "$out"
[[ "$(sends)" == 0 ]] && ! grep -q '^init-cursor ' "$WORK/ops_calls" && pass "nothing was sent after NonceAdvanced" || fail "nothing was sent after NonceAdvanced" "$(cat "$WORK/ops_calls")"

# No sequencer running: register starts it (docker compose up -d sequencer), waits for it, then registers.
: > "$WORK/ops_calls"; : > "$WORK/docker_calls"; curl_stub none; : > "$WORK/start_answers"
if out="$(REGISTER_SEQUENCER_WAIT_SECS=5 "$ROLLUP" register --confirm 2>&1)"; then pass "register starts a sequencer that is not running and then registers"; else fail "register starts the sequencer" "$out"; fi
grep -q 'compose .*up -d sequencer' "$WORK/docker_calls" && pass "register ran: docker compose up -d sequencer" || fail "register starts the sequencer" "$(cat "$WORK/docker_calls")"
[[ "$(sends)" == 1 ]] && pass "registration was sent once after the sequencer came up" || fail "registration was sent once after the sequencer came up" "$(cat "$WORK/ops_calls")"
rm -f "$WORK/start_answers" "$ROLLUP_OUT/pdas.env"

# A sequencer that never answers: refused by name before the send.
: > "$WORK/ops_calls"; : > "$WORK/docker_calls"; curl_stub none
if out="$(REGISTER_SEQUENCER_WAIT_SECS=2 "$ROLLUP" register --confirm 2>&1)"; then fail "register refuses without a sequencer" "exited 0"
elif grep -q '^SequencerNotReachable' <<<"$out"; then pass "register refuses by name (SequencerNotReachable) when the sequencer never answers"; else fail "SequencerNotReachable" "$out"; fi
[[ "$(sends)" == 0 ]] && pass "nothing was sent without a sequencer" || fail "nothing was sent without a sequencer" "$(cat "$WORK/ops_calls")"
curl_stub 0x1
if out="$("$ROLLUP" register --confirm 2>&1)"; then fail "register refuses a sequencer on another chain id" "exited 0"
elif grep -q '^ChainIdMismatch' <<<"$out"; then pass "register refuses by name (ChainIdMismatch) when the sequencer reports another id"; else fail "ChainIdMismatch" "$out"; fi

# The program registered another id than init derived (a race with a second registration by the same key).
curl_stub 0x100067932
if out="$(STUB_REGISTERED_ID=4295391999 "$ROLLUP" register --confirm 2>&1)"; then fail "register refuses a registered id that differs from the genesis" "exited 0"
elif grep -q '^NonceAdvanced' <<<"$out"; then pass "register names NonceAdvanced when the registered id differs from the genesis"; else fail "registered id differs" "$out"; fi

# No record from init.
rm -f "$ROLLUP_OUT/chain-id.env"
if out="$("$ROLLUP" register --dry-run 2>&1)"; then fail "register without init's record refuses" "exited 0"
elif grep -q '^NotInitialised' <<<"$out"; then pass "register refuses by name (NotInitialised) without the id record"; else fail "register without the record" "$out"; fi
finish register_confirm
