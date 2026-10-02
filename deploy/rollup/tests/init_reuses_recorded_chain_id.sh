#!/usr/bin/env bash
# deploy/rollup/tests/init_reuses_recorded_chain_id.sh — once init has recorded a chain id (rendered/chain-id.env), later
# inits reuse it. Registering moves the payer's nonce forward, and the id derived from the new nonce would be another
# chain: init must not follow it. Every later change (the prover, an image tag) goes through init.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
ROME_ZK_TAG=t1 "$ROLLUP" init >/dev/null 2>&1
make_register_stubs
printf 'ab%.0s' $(seq 32) > "$WORK/keys/sequencer.key"
"$ROLLUP" register --confirm >/dev/null 2>&1
G="$ROLLUP_OUT/genesis.json"; cp "$G" "$WORK/genesis.before"

# The prover goes on, with a new tag, after registering (the payer's nonce is now 1).
echo '{"elf_sha256":"0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"}' > "$WORK/vkey.json"
printf 'PROVER=on\nVKEY_JSON=%s\n' "$WORK/vkey.json" >> "$ROLLUP_ENV"
if out="$(STUB_NONCE=1 ROME_ZK_TAG=t2 "$ROLLUP" init 2>&1)"; then pass "init after register succeeds with the nonce moved"; else fail "init after register succeeds" "$out"; fi
cmp -s "$G" "$WORK/genesis.before" && pass "genesis.json is byte-identical" || fail "genesis.json is byte-identical" "$(diff "$G" "$WORK/genesis.before" | head -5)"
grep -qE '^chain_id = 4295391538$' "$ROLLUP_OUT/prover.toml" 2>/dev/null && pass "prover.toml rendered with the recorded chain id" || fail "prover.toml rendered with the recorded chain id" "$(ls "$ROLLUP_OUT")"
grep -q '^ROME_ZK_TAG=t2$' "$ROLLUP_OUT/compose.env" && pass "compose.env carries the new image tag" || fail "compose.env carries the new image tag" "$(grep TAG "$ROLLUP_OUT/compose.env")"
grep -q '^CHAIN_ID=4295391538$' "$ROLLUP_OUT/compose.env" && grep -q '^NONCE=0$' "$ROLLUP_OUT/chain-id.env" && grep -q '^CHAIN_ID=4295391538$' "$ROLLUP_OUT/chain-id.env" && pass "the record keeps its nonce and chain id" || fail "the record keeps its nonce and chain id" "$(cat "$ROLLUP_OUT/chain-id.env")"

# A different payer key than the one that was recorded: refused by name, nothing changes.
cp "$ROLLUP_OUT/compose.env" "$WORK/compose.before"
if out="$(STUB_AUTHORITY=SomeOtherKey ROME_ZK_TAG=t3 "$ROLLUP" init 2>&1)"; then fail "init refuses a payer key that is not the recorded authority" "exited 0"
elif grep -q '^AuthorityChanged' <<<"$out"; then pass "init refuses by name (AuthorityChanged) when the payer key is not the recorded authority"; else fail "AuthorityChanged" "$out"; fi
cmp -s "$ROLLUP_OUT/compose.env" "$WORK/compose.before" && pass "nothing was rewritten after AuthorityChanged" || fail "nothing was rewritten after AuthorityChanged" "compose.env changed"

# No record: the id is derived again, and a genesis that is already there refuses, naming the id change.
rm -f "$ROLLUP_OUT/chain-id.env"
if out="$(STUB_NONCE=1 "$ROLLUP" init 2>&1)"; then fail "init without a record on a moved nonce refuses" "exited 0"
elif grep -q '^GenesisDrift' <<<"$out" && grep -q 'chain id' <<<"$out" && grep -q '4295391539' <<<"$out"; then pass "GenesisDrift names the chain id change when that is the cause"; else fail "GenesisDrift names the id change" "$out"; fi
finish init_reuses_recorded_chain_id
