#!/usr/bin/env bash
# deploy/rollup/tests/init_refuses_chain_id_not_wallet_safe.sh — about half of all derived chain ids are above MetaMask's
# MAX_SAFE_CHAIN_ID (4503599627370476), so wallets like MetaMask cannot add those chains. `init` refuses such an id by name
# before the chain is registered (the id is permanent after that). For a chain that is already registered it only warns.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture

MAX=4503599627370476
AUTH=StubAuthority1111111111111111111111111111111

# One above the bound: refused by name, the message says what to do, nothing is rendered and nothing is sent.
if out="$(STUB_ID=$((MAX + 1)) "$ROLLUP" init 2>&1)"; then fail "init refuses a derived id above $MAX" "exited 0"
elif grep -q '^ChainIdNotWalletSafe' <<<"$out"; then pass "init refuses by name (ChainIdNotWalletSafe) a derived id one above $MAX"; else fail "ChainIdNotWalletSafe" "$out"; fi
grep -q "$((MAX + 1))" <<<"$out" && grep -q 'MetaMask' <<<"$out" && grep -q 'solana-keygen new' <<<"$out" && grep -q 'nothing has been sent' <<<"$out" \
  && pass "the message names the id and MetaMask, says to run solana-keygen new, and says nothing was sent" || fail "the refusal message" "$out"
[[ ! -e "$ROLLUP_OUT/genesis.json" && ! -e "$ROLLUP_OUT/chain-id.env" ]] && pass "nothing was rendered or recorded after the refusal" || fail "nothing was rendered or recorded after the refusal" "$(ls "$ROLLUP_OUT" 2>&1)"
[[ "$(sends)" == 0 ]] && pass "no registration was sent" || fail "no registration was sent" "$(cat "$WORK/cargo_calls")"

# The id at the bound is accepted; the largest id the program can derive (2^53 - 1) is refused.
if out="$(STUB_ID=$MAX ROLLUP_OUT="$WORK/o-max" "$ROLLUP" init 2>&1)"; then pass "init accepts a derived id exactly at $MAX"; else fail "init accepts an id exactly at $MAX" "$out"; fi
grep -q "^CHAIN_ID=$MAX\$" "$WORK/o-max/chain-id.env" 2>/dev/null && pass "the id at the bound is recorded" || fail "the id at the bound is recorded" "$(cat "$WORK/o-max/chain-id.env" 2>&1)"
if out="$(STUB_ID=9007199254740991 ROLLUP_OUT="$WORK/o-top" "$ROLLUP" init 2>&1)"; then fail "init refuses 2^53 - 1" "exited 0"
elif grep -q '^ChainIdNotWalletSafe' <<<"$out"; then pass "init refuses by name (ChainIdNotWalletSafe) the largest derivable id, 2^53 - 1"; else fail "ChainIdNotWalletSafe for 2^53 - 1" "$out"; fi

# A chain that is already registered: its id is fixed, so init warns once by name and carries on.
unsafe=$((MAX + 5))
record() { # $1=dir, $2=nonce: an earlier init's record for the same payer with an unsafe id
  mkdir -p "$1"; printf 'AUTHORITY=%s\nNONCE=%s\nCHAIN_ID=%s\n' "$AUTH" "$2" "$unsafe" > "$1/chain-id.env"
}
# (1) rendered/pdas.env exists: `register` finished and wrote it.
record "$WORK/o-reg" 0; printf 'CURSOR_PDA=c\nROOT_PDA=r\n' > "$WORK/o-reg/pdas.env"
out="$(ROLLUP_OUT="$WORK/o-reg" "$ROLLUP" init 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "init does not refuse an unsafe id on a chain whose pdas.env exists" || fail "init does not refuse a registered chain" "rc=$rc $out"
[[ "$(grep -c 'ChainIdNotWalletSafe' <<<"$out")" == 1 ]] && grep -q '^ChainIdNotWalletSafe' <<<"$out" && pass "it prints the ChainIdNotWalletSafe warning once" || fail "one ChainIdNotWalletSafe warning" "$out"
grep -q "\"chainId\": *$unsafe" "$WORK/o-reg/genesis.json" 2>/dev/null && pass "the genesis carries the registered (unsafe) id" || fail "the genesis carries the registered id" "$(ls "$WORK/o-reg")"
# (2) the payer's nonce has moved past the recorded one (registration moved it) although register stopped before pdas.env.
record "$WORK/o-moved" 0
out="$(STUB_NONCE=1 ROLLUP_OUT="$WORK/o-moved" "$ROLLUP" init 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && grep -q '^ChainIdNotWalletSafe' <<<"$out" && pass "a moved nonce on a recorded id counts as registered: warning, no refusal" || fail "a moved nonce counts as registered" "rc=$rc $out"
# (3) a record with neither sign of registration is still refused.
record "$WORK/o-unreg" 0
if out="$(ROLLUP_OUT="$WORK/o-unreg" "$ROLLUP" init 2>&1)"; then fail "an unregistered recorded unsafe id is refused" "exited 0"
elif grep -q '^ChainIdNotWalletSafe' <<<"$out"; then pass "a recorded unsafe id with no pdas.env and an unmoved nonce is refused"; else fail "ChainIdNotWalletSafe for an unregistered record" "$out"; fi
# A safe id never warns.
out="$(ROLLUP_OUT="$WORK/o-safe" "$ROLLUP" init 2>&1)" && ! grep -q 'ChainIdNotWalletSafe' <<<"$out" && pass "a safe id prints no ChainIdNotWalletSafe" || fail "a safe id prints no warning" "$out"
finish init_refuses_chain_id_not_wallet_safe
