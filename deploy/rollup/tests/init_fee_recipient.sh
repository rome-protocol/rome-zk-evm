#!/usr/bin/env bash
# deploy/rollup/tests/init_fee_recipient.sh — chain.toml must name genesis.fee_recipient, and the rendered genesis coinbase
# is that address (the sequencer, derive and the guest all take the chain's fee recipient from the genesis coinbase, and the
# genesis cannot change after registration). Refusals are by name, with the same rules as genesis.backed_address.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture

refuses() { # $1=label $2=expected name $3=chain.toml
  local label="$1" name="$2" toml="$3"
  : > "$WORK/ops_calls"; rm -rf "$WORK/o-r"
  if out="$(CHAIN_TOML="$toml" ROLLUP_OUT="$WORK/o-r" "$ROLLUP" init 2>&1)"; then fail "init refuses: $label" "exited 0"
  elif grep -q "^$name" <<<"$out" && grep -q 'fee_recipient' <<<"$out"; then pass "init refuses by name ($name): $label"
  else fail "init refuses by name ($name): $label" "$out"; fi
  [[ ! -e "$WORK/o-r/genesis.json" ]] || fail "nothing rendered after $name ($label)" "genesis.json exists"
  grep -q 'chain-id' "$WORK/ops_calls" && fail "no network call after $name ($label)" "$(cat "$WORK/ops_calls")"
}

# Missing: the key is absent, or empty.
grep -v '^fee_recipient' "$FIX/chain.toml" > "$WORK/missing.toml"
refuses "fee_recipient absent" FeeRecipientMissing "$WORK/missing.toml"
sed 's/^fee_recipient = .*/fee_recipient = ""/' "$FIX/chain.toml" > "$WORK/empty.toml"
refuses "fee_recipient empty" FeeRecipientMissing "$WORK/empty.toml"

# Malformed.
n=0
for bad in nope 0x1234 0x11111111111111111111111111111111111111 111111111111111111111111111111111111111111 0x11111111111111111111111111111111111111zz 0x111111111111111111111111111111111111111111; do
  n=$((n+1)); sed "s/^fee_recipient = .*/fee_recipient = \"$bad\"/" "$FIX/chain.toml" > "$WORK/inv$n.toml"
  refuses "fee_recipient '$bad'" FeeRecipientInvalid "$WORK/inv$n.toml"
done

# Reserved: zero, the precompile range 0x00..00 to 0x00..ff, and the exit portal.
for addr in 0x0000000000000000000000000000000000000000 0x0000000000000000000000000000000000000001 0x000000000000000000000000000000000000000A 0x00000000000000000000000000000000000000ff 0x4200000000000000000000000000000000000016; do
  n=$((n+1)); sed "s/^fee_recipient = .*/fee_recipient = \"$addr\"/" "$FIX/chain.toml" > "$WORK/res$n.toml"
  refuses "fee_recipient $addr" FeeRecipientReserved "$WORK/res$n.toml"
done
sed 's/^fee_recipient = .*/fee_recipient = "0x0000000000000000000000000000000000000100"/' "$FIX/chain.toml" > "$WORK/ok100.toml"
CHAIN_TOML="$WORK/ok100.toml" ROLLUP_OUT="$WORK/o-ok100" "$ROLLUP" init >/dev/null 2>&1 && pass "a fee recipient just above the precompile range (0x..0100) is accepted" || fail "0x..0100 is accepted" "init refused it"

# The rendered genesis coinbase is the fee recipient, as written, and is not zero.
want="$(sed -n 's/^fee_recipient = "\(.*\)"/\1/p' "$FIX/chain.toml")"
"$ROLLUP" init >/dev/null 2>&1 || fail "init succeeds against the fixture" "refused"
python3 - "$ROLLUP_OUT/genesis.json" "$want" <<'PY' && pass "genesis.json coinbase equals genesis.fee_recipient ($want)" || fail "genesis.json coinbase equals genesis.fee_recipient" "see above"
import json, sys
g = json.load(open(sys.argv[1]))
assert g["coinbase"].lower() == sys.argv[2].lower(), g["coinbase"]
assert int(g["coinbase"], 16) != 0, "coinbase is still zero"
PY
# A mixed-case address is rendered as written.
sed 's/^fee_recipient = .*/fee_recipient = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01"/' "$FIX/chain.toml" > "$WORK/mixed.toml"
CHAIN_TOML="$WORK/mixed.toml" ROLLUP_OUT="$WORK/o-mixed" "$ROLLUP" init >/dev/null 2>&1
grep -q '"coinbase": "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01"' "$WORK/o-mixed/genesis.json" && pass "a mixed-case fee recipient is rendered as written" || fail "a mixed-case fee recipient is rendered as written" "$(grep coinbase "$WORK/o-mixed/genesis.json" 2>&1)"

# The genesis cannot change after registration, so a changed fee recipient is GenesisDrift, and the coinbase is untouched.
sed 's/^fee_recipient = .*/fee_recipient = "0x4444444444444444444444444444444444444444"/' "$FIX/chain.toml" > "$WORK/changed.toml"
if out="$(CHAIN_TOML="$WORK/changed.toml" "$ROLLUP" init 2>&1)"; then fail "a changed fee recipient is refused as GenesisDrift" "exited 0"
elif grep -q '^GenesisDrift' <<<"$out"; then pass "a changed fee recipient on an existing genesis is refused by name (GenesisDrift)"; else fail "GenesisDrift" "$out"; fi
grep -q "\"coinbase\": \"$want\"" "$ROLLUP_OUT/genesis.json" && pass "the existing genesis keeps its coinbase after the refusal" || fail "the existing genesis keeps its coinbase" "$(grep coinbase "$ROLLUP_OUT/genesis.json")"

# The example file names the key, and its placeholder is refused (the operator must choose one).
grep -q '^fee_recipient = "0x0000000000000000000000000000000000000000"' "$ROLLUP_DIR/chain.toml.example" && pass "chain.toml.example has the fee_recipient key (a placeholder init refuses)" || fail "chain.toml.example has fee_recipient" "missing"
cp "$ROLLUP_DIR/chain.toml.example" "$WORK/example.toml"
refuses "the example's placeholder fee_recipient" FeeRecipientReserved "$WORK/example.toml"
finish init_fee_recipient
