#!/usr/bin/env bash
# deploy/rollup/tests/init_genesis_zero_backed.sh — a chain's genesis is fixed at registration, so a genesis that mints
# native supply can never take deposits safely (its holder could exit coins that other people's deposits paid for).
# `rollup init` therefore renders a genesis with no balances unless chain.toml declares ONE backed balance
# (genesis.backed_address + genesis.backed_balance_lamports), which the operator locks in the chain's vault with
# zk-bridge Fund before Rome registers the key. wei = lamports x 1e9 (wrapped SOL, 1 lamport = 1 gwei).
# Covers: the zero-balance default, the backed render (exact wei), every refusal by name, the old funded_address
# key refused, what init prints for a backed balance, and the example file.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
OUT="$ROLLUP_OUT"
PORTAL=0x4200000000000000000000000000000000000016

# ---- helpers -------------------------------------------------------------------------------------------------------
# genesis_check GENESIS EXPECTED_ALLOC_JSON -> the genesis alloc is exactly the expected backed entries plus the exit portal
# predeploy (balance 0x0, code present), and every balance other than the expected ones is zero.
alloc_is() { # $1=genesis.json $2=JSON object {address: expected wei hex} for the non-portal entries (may be {})
  python3 - "$1" "$2" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
want = json.loads(sys.argv[2])
portal = "0x4200000000000000000000000000000000000016"
alloc = g["alloc"]
assert set(alloc) == set(want) | {portal}, (sorted(alloc), sorted(want))
assert alloc[portal]["balance"] == "0x0" and len(alloc[portal]["code"]) > 100, alloc[portal]
for addr, wei in want.items():
    assert alloc[addr] == {"balance": wei}, (addr, alloc[addr], wei)
PY
}
# toml_with GENESIS_LINES... -> writes $WORK/t.toml: the fixture with the given lines added under [genesis]
toml_with() {
  python3 - "$FIX/chain.toml" "$WORK/t.toml" "$@" <<'PY'
import sys
src, dst, lines = sys.argv[1], sys.argv[2], sys.argv[3:]
s = open(src).read()
open(dst, "w").write(s.replace("[genesis]\n", "[genesis]\n" + "".join(l + "\n" for l in lines), 1))
PY
}
run_init() { # $1=output dir; chain.toml is $WORK/t.toml; prints init's output; leaves the stub's call log in $WORK/cargo_calls
  : > "$WORK/cargo_calls"; rm -rf "$1"
  CHAIN_TOML="$WORK/t.toml" ROLLUP_OUT="$1" "$ROLLUP" init 2>&1
}
refuses() { # $1=label $2=expected name $3=text the message must also contain (a key name or the replacement), rest = genesis lines
  local label="$1" name="$2" must="$3"; shift 3
  toml_with "$@"
  if out="$(run_init "$WORK/o-r")"; then fail "init refuses: $label" "exited 0"
  elif grep -q "^$name" <<<"$out" && grep -q -- "$must" <<<"$out"; then pass "init refuses by name ($name): $label"
  else fail "init refuses by name ($name): $label" "$out"; fi
  [[ ! -e "$WORK/o-r/genesis.json" ]] || fail "nothing rendered after $name ($label)" "genesis.json exists"
  grep -q 'chain-id' "$WORK/cargo_calls" && fail "no network call after $name ($label)" "$(cat "$WORK/cargo_calls")"
}

# ---- the default: zero balances ------------------------------------------------------------------------------------
if out="$("$ROLLUP" init 2>&1)"; then pass "init succeeds against the fixture, which sets no balance"; else fail "init succeeds against the fixture" "$out"; finish init_genesis_zero_backed; fi
alloc_is "$OUT/genesis.json" '{}' && pass "the default genesis has no balances: the only alloc entry is the exit portal predeploy at balance 0" || fail "the default genesis has no balances" "see above"
grep -q '0x33b2e3c9fd0803ce8000000' "$OUT/genesis.json" && fail "the old 1e27 wei mint is gone" "still in genesis.json" || pass "the old 1e27 wei mint is gone from the genesis"
if grep -q 'lock exactly\|zk-bridge' <<<"$out"; then fail "a zero-balance init prints nothing about locking funds" "$out"; else pass "a zero-balance init prints nothing about a backed balance or Fund"; fi

# ---- a backed balance: exactly one alloc entry, exact wei ---------------------------------------------------------------
B=0x1111111111111111111111111111111111111111
toml_with "backed_address = \"$B\"" "backed_balance_lamports = 1_500_000_000"
out="$(run_init "$WORK/o-b1")" && pass "init succeeds with a backed address and 1,500,000,000 lamports" || fail "init succeeds with a backed balance" "$out"
alloc_is "$WORK/o-b1/genesis.json" "{\"$B\": \"0x14d1120d7b160000\"}" && pass "1,500,000,000 lamports render as exactly 1.5e18 wei (0x14d1120d7b160000), one entry beside the portal" || fail "the backed entry is exact" "see above"
grep -q "\"coinbase\": \"0x3333333333333333333333333333333333333333\"" "$WORK/o-b1/genesis.json" && pass "the coinbase is still the fee recipient" || fail "the coinbase is still the fee recipient" "$(grep coinbase "$WORK/o-b1/genesis.json")"

toml_with "backed_address = \"$B\"" "backed_balance_lamports = 1"
run_init "$WORK/o-b2" >/dev/null; alloc_is "$WORK/o-b2/genesis.json" "{\"$B\": \"0x3b9aca00\"}" && pass "1 lamport renders as exactly 1 gwei (0x3b9aca00)" || fail "1 lamport is 1 gwei" "see above"
toml_with "backed_address = \"$B\"" "backed_balance_lamports = 18446744073709551615"
run_init "$WORK/o-b3" >/dev/null; alloc_is "$WORK/o-b3/genesis.json" "{\"$B\": \"0x3b9ac9ffffffffffc4653600\"}" && pass "the largest u64 lamport amount renders exactly (u64 max x 1e9)" || fail "u64 max renders exactly" "see above"
toml_with 'backed_address = "0xAbCdEf0123456789aBcDeF0123456789AbCdEf01"' "backed_balance_lamports = 2000000000"
run_init "$WORK/o-b4" >/dev/null; grep -q '"0xAbCdEf0123456789aBcDeF0123456789AbCdEf01"' "$WORK/o-b4/genesis.json" && pass "a mixed-case backed address is rendered as written" || fail "a mixed-case backed address is rendered as written" "$(grep -n 0xAbC "$WORK/o-b4/genesis.json")"
toml_with 'backed_address = "0x0000000000000000000000000000000000000100"' "backed_balance_lamports = 1000000000"
run_init "$WORK/o-b5" >/dev/null && pass "a backed address just above the precompile range (0x..0100) is accepted" || fail "0x..0100 is accepted" "init refused it"

# ---- what init prints for a backed balance ----------------------------------------------------------------------------------
toml_with "backed_address = \"$B\"" "backed_balance_lamports = 1500000000"
out="$(run_init "$WORK/o-p")"
grep -q 'lock exactly 1500000000 lamports' <<<"$out" && pass "init tells the operator the exact lamport amount to lock" || fail "init prints the exact lamport amount" "$out"
grep -q 'Fund' <<<"$out" && grep -qi 'before' <<<"$out" && grep -qi 'register' <<<"$out" && pass "init says to lock it with Fund before asking Rome to register the key" || fail "init names Fund and the order" "$out"
grep -q "$B" <<<"$out" && pass "init names the backed address" || fail "init names the backed address" "$out"
grep -qi "create your chain's vault for the wrapped SOL mint" <<<"$out" && pass "init says how to create the vault on the shared zk-bridge" || fail "init says how to create the vault on the shared zk-bridge" "$out"

# ---- refusals by name ---------------------------------------------------------------------------------------------------
# The old key. A chain.toml that still sets funded_address is refused, and the message says what replaced it.
refuses "funded_address still set" FundedAddressRemoved backed_balance_lamports 'funded_address = "0x1111111111111111111111111111111111111111"'
refuses "funded_address set, with the new keys beside it" FundedAddressRemoved backed_address 'funded_address = "0x1111111111111111111111111111111111111111"' "backed_address = \"$B\"" "backed_balance_lamports = 1000000000"
refuses "funded_address empty" FundedAddressRemoved backed_address 'funded_address = ""'
refuses "funded_address as the old placeholder" FundedAddressRemoved funded_address 'funded_address = "0x0000000000000000000000000000000000000000"'

# A key init does not know under [genesis]; the likely mistake is a balance without its unit.
refuses "backed_balance without the unit" GenesisKeyUnknown backed_balance_lamports "backed_address = \"$B\"" "backed_balance = 1000000000"
refuses "a typo'd [genesis] key" GenesisKeyUnknown fee_recipient 'fee_recpient = "0x3333333333333333333333333333333333333333"'

# One without the other.
refuses "an address without a balance" BackedBalanceMissing backed_balance_lamports "backed_address = \"$B\""
refuses "a balance without an address" BackedAddressMissing backed_address "backed_balance_lamports = 1000000000"
refuses "an empty backed_address with a balance" BackedAddressMissing backed_address 'backed_address = ""' "backed_balance_lamports = 1000000000"

# Malformed address.
n=0
for bad in nope 0x1234 0x11111111111111111111111111111111111111 111111111111111111111111111111111111111111 0x11111111111111111111111111111111111111zz 0x111111111111111111111111111111111111111111; do
  n=$((n+1)); refuses "backed_address '$bad'" BackedAddressInvalid backed_address "backed_address = \"$bad\"" "backed_balance_lamports = 1000000000"
done
# Reserved: zero, the precompile range 0x00..00 to 0x00..ff, and the exit portal.
for addr in 0x0000000000000000000000000000000000000000 0x0000000000000000000000000000000000000001 0x000000000000000000000000000000000000000A 0x00000000000000000000000000000000000000ff $PORTAL; do
  refuses "backed_address $addr" BackedAddressReserved backed_address "backed_address = \"$addr\"" "backed_balance_lamports = 1000000000"
done

# A balance that is not a whole number of lamports in 1..u64 max, or not written as a TOML integer.
refuses "zero lamports" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 0"
refuses "negative lamports" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = -5"
refuses "a fraction of a lamport" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 1.5"
refuses "a value in SOL written as a decimal" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 0.5"
refuses "scientific notation" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 1e9"
refuses "a quoted number" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" 'backed_balance_lamports = "1000000000"'
refuses "a value with a unit" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" 'backed_balance_lamports = "1 SOL"'
refuses "a hex number" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 0x3b9aca00"
refuses "more than u64 max lamports" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = 18446744073709551616"
refuses "a boolean" BackedBalanceInvalid backed_balance_lamports "backed_address = \"$B\"" "backed_balance_lamports = true"

# ---- the genesis cannot change after registration -------------------------------------------------------------------------
# A zero-balance genesis that init already wrote is not silently turned into a backed one by editing chain.toml.
toml_with "backed_address = \"$B\"" "backed_balance_lamports = 1000000000"
if out="$(CHAIN_TOML="$WORK/t.toml" "$ROLLUP" init 2>&1)"; then fail "adding a backed balance to an existing zero genesis is refused" "exited 0"
elif grep -q '^GenesisDrift' <<<"$out"; then pass "adding a backed balance to an existing genesis is refused by name (GenesisDrift)"; else fail "GenesisDrift" "$out"; fi
alloc_is "$OUT/genesis.json" '{}' && pass "the existing zero-balance genesis is untouched after the refusal" || fail "the existing genesis is untouched" "see above"

# ---- the example file ---------------------------------------------------------------------------------------------------------
EX="$ROLLUP_DIR/chain.toml.example"
grep -qE '^\s*funded_address' "$EX" && fail "chain.toml.example no longer sets funded_address" "still there" || pass "chain.toml.example no longer sets funded_address"
grep -qE '^# backed_address = ' "$EX" && grep -qE '^# backed_balance_lamports = ' "$EX" && pass "chain.toml.example shows the backed keys, commented out (zero balances by default)" || fail "chain.toml.example shows the backed keys, commented out" "missing"
sed 's/^fee_recipient = .*/fee_recipient = "0x3333333333333333333333333333333333333333"/' "$EX" > "$WORK/ex1.toml"
CHAIN_TOML="$WORK/ex1.toml" ROLLUP_OUT="$WORK/o-ex1" "$ROLLUP" init >/dev/null 2>&1 && alloc_is "$WORK/o-ex1/genesis.json" '{}' && pass "the example with only a fee recipient set renders a zero-balance genesis" || fail "the example with only a fee recipient renders zero balances" "init refused it or the alloc differs"
sed -e 's/^fee_recipient = .*/fee_recipient = "0x3333333333333333333333333333333333333333"/' \
    -e 's/^# backed_address = .*/backed_address = "0x1111111111111111111111111111111111111111"/' \
    -e 's/^# backed_balance_lamports = .*/backed_balance_lamports = 2_000_000_000/' "$EX" > "$WORK/ex2.toml"
CHAIN_TOML="$WORK/ex2.toml" ROLLUP_OUT="$WORK/o-ex2" "$ROLLUP" init >/dev/null 2>&1 && alloc_is "$WORK/o-ex2/genesis.json" "{\"$B\": \"0x1bc16d674ec80000\"}" && pass "uncommenting the example's backed keys renders one entry of 2e18 wei" || fail "the example's backed keys render" "init refused it or the alloc differs"

finish init_genesis_zero_backed
