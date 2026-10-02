#!/usr/bin/env bash
# deploy/rollup/tests/init_renders_every_config_from_chain_toml.sh — `rollup init` turns chain.toml (plus the
# machine's .env and the programs file) into every config the stack mounts: the sequencer config, the genesis,
# the batcher and derive configs, the engine-API secret and the compose variables. Every value comes from the
# fixture, which differs from every default, so a render that ignores chain.toml fails here. Refusals are by name.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
OUT="$ROLLUP_OUT"

if out="$("$ROLLUP" init 2>&1)"; then pass "rollup init succeeds against the fixture chain.toml"; else fail "rollup init succeeds against the fixture chain.toml" "$out"; finish init_renders_every_config_from_chain_toml; fi

for f in sequencer-config.toml genesis.json batcher.toml derive.toml jwt.hex compose.env chain-id.env; do
  [[ -s "$OUT/$f" ]] && pass "rendered $f exists" || fail "rendered $f exists" "missing under $OUT"
done

# The chain id is read from Solana (stubbed here: authority StubAuthority..., nonce 0 -> 4295391538), not chosen.
grep -q 'chain-id --keypair '"$WORK/keys/payer.json"' --settlement FixtureBprogram11111111111111111111111111112 --rpc-url https://rpc.example.invalid' "$WORK/cargo_calls" && pass "init asked the chain-id command for the payer's id, with the settlement program and RPC from the config" || fail "init calls chain-id" "$(cat "$WORK/cargo_calls" 2>/dev/null)"
has() { grep -qE -- "$2" "$OUT/$1" && pass "$1: $3" || fail "$1: $3" "no match for /$2/ in $(head -c 300 "$OUT/$1" 2>/dev/null)"; }
has sequencer-config.toml '^chain_id = 4295391538$' "chain_id is the derived id"
has chain-id.env '^AUTHORITY=StubAuthority1111111111111111111111111111111$' "the authority is recorded"
has chain-id.env '^NONCE=0$' "the nonce is recorded"
has chain-id.env '^CHAIN_ID=4295391538$' "the derived id is recorded"
has sequencer-config.toml '^blocks_per_batch = 30$' "blocks_per_batch comes from chain.toml"
has sequencer-config.toml '^sub_blocks_per_block = 10$' "sub_blocks_per_block comes from chain.toml"
has sequencer-config.toml '^sub_block_gas_limit = 3000000$' "sub_block_gas_limit comes from chain.toml"
has sequencer-config.toml '^sub_block_ms = 40$' "sub_block_ms comes from chain.toml"
has sequencer-config.toml '^empty_block_interval_secs = 5$' "empty_block_interval_secs comes from chain.toml"
has sequencer-config.toml '^sequencer_key_path = "/data/sequencer.key"$' "container key path"
has sequencer-config.toml '^log_dir = "/data/reth/log"$' "container log dir (the batcher reads it)"

python3 - "$OUT/genesis.json" "$REPO_ROOT/contracts/exit-portal/RomeExitPortal.runtime.hex" <<'PY' && pass "genesis.json: chainId is the derived id, gas limit and funded account come from chain.toml; the exit portal is the one predeploy" || fail "genesis.json: chainId is the derived id, gas limit and funded account come from chain.toml; the exit portal is the one predeploy" "see above"
import json, sys
g = json.load(open(sys.argv[1]))
assert g["config"]["chainId"] == 4295391538, g["config"]["chainId"]
assert int(g["gasLimit"], 16) == 30_000_000, g["gasLimit"]
assert list(g["alloc"]) == ["0x1111111111111111111111111111111111111111", "0x4200000000000000000000000000000000000016"], list(g["alloc"])
code = g["alloc"]["0x4200000000000000000000000000000000000016"]["code"]
want = "0x" + "".join(open(sys.argv[2]).read().split()).removeprefix("0x")
assert code == want and len(code) > 100, "exit portal predeploy differs from contracts/exit-portal/RomeExitPortal.runtime.hex"
PY

has batcher.toml '^chain_id = 4295391538$' "chain_id"
has batcher.toml '^inbox_program_id = "FixtureAprogram11111111111111111111111111112"$' "inbox program id comes from the programs file"
has batcher.toml '^settlement_program_id = "FixtureBprogram11111111111111111111111111112"$' "settlement program id comes from the programs file"
has batcher.toml '^rpc_url = "https://rpc.example.invalid"$' "Solana RPC comes from .env"
has batcher.toml '^batch_close_after_secs = 45$' "batch_close_after_secs comes from chain.toml"
has batcher.toml '^cluster = "devnet"$' "cluster comes from the programs file"
has batcher.toml '^log_dir = "/data/reth/log"$' "log dir matches the sequencer's"

has derive.toml '^chain_id = 4295391538$' "chain_id"
has derive.toml '^inbox_program_id = "FixtureAprogram11111111111111111111111111112"$' "inbox program id"
has derive.toml '^settlement_program_id = "FixtureBprogram11111111111111111111111111112"$' "settlement program id"
has derive.toml '^blocks_per_batch = 30$' "blocks_per_batch is the sequencer's own value, not a second copy"
has derive.toml '^rpc_url = "http://reth-verifier:8547"$' "verifier RPC is reached on the compose network"

has compose.env '^CHAIN_ID=4295391538$' "CHAIN_ID"
has compose.env '^BLOCK_GAS_LIMIT=30000000$' "BLOCK_GAS_LIMIT = sub_block_gas_limit x sub_blocks_per_block"
has compose.env '^BLOCKS_PER_BATCH=30$' "BLOCKS_PER_BATCH"
has compose.env '^BATCH_CLOSE_AFTER_SECS=45$' "BATCH_CLOSE_AFTER_SECS"

if grep -rqE '__[A-Z0-9_]+__' "$OUT"; then fail "no template placeholder is left in any rendered file" "$(grep -rnE '__[A-Z0-9_]+__' "$OUT" | head -3)"; else pass "no template placeholder is left in any rendered file"; fi

jwt="$(cat "$OUT/jwt.hex")"
[[ "$jwt" =~ ^[0-9a-f]{64}$ ]] && pass "jwt.hex is 64 hex characters" || fail "jwt.hex is 64 hex characters" "got ${#jwt} chars"
mode="$(stat -c '%a' "$OUT/jwt.hex" 2>/dev/null || stat -f '%Lp' "$OUT/jwt.hex")"
[[ "$mode" == "600" || "$mode" == "644" ]] && pass "jwt.hex is not group/world writable (mode $mode)" || fail "jwt.hex mode" "$mode"

# Re-running changes nothing: the engine secret and the genesis are kept byte-for-byte.
before="$(cd "$OUT" && cat jwt.hex genesis.json sequencer-config.toml | shasum)"
"$ROLLUP" init >/dev/null 2>&1
after="$(cd "$OUT" && cat jwt.hex genesis.json sequencer-config.toml | shasum)"
[[ "$before" == "$after" ]] && pass "a second init changes nothing (engine secret and genesis kept)" || fail "a second init changes nothing" "outputs differ"

# A changed chain.toml must not silently rewrite a genesis that may already be live.
sed -i.bak 's/0x1111111111111111111111111111111111111111/0x2222222222222222222222222222222222222222/' "$WORK/chain.toml"
if out="$("$ROLLUP" init 2>&1)"; then fail "init refuses to overwrite a differing genesis" "exited 0: $out"
elif grep -q 'GenesisDrift' <<<"$out"; then pass "init refuses by name (GenesisDrift) when chain.toml would change an existing genesis"
else fail "init refuses by name (GenesisDrift)" "$out"; fi
grep -q 0x1111111111111111111111111111111111111111 "$OUT/genesis.json" && pass "the existing genesis is untouched after the refusal" || fail "the existing genesis is untouched" "changed"
cp "$FIX/chain.toml" "$WORK/chain.toml"

# A payer whose nonce moved since init: the recorded id is reused, so the rendered genesis does not change.
if out="$(STUB_NONCE=1 "$ROLLUP" init 2>&1)"; then pass "a moved nonce on an existing output directory keeps the recorded id"
else fail "a moved nonce keeps the recorded id" "$out"; fi
grep -q '"chainId": *4295391538' "$OUT/genesis.json" && pass "the genesis still carries the recorded id" || fail "the genesis still carries the recorded id" "changed"
grep -q '^CHAIN_ID=4295391538$' "$OUT/chain-id.env" && pass "the recorded id is untouched after that run" || fail "the recorded id is untouched" "$(cat "$OUT/chain-id.env")"
STUB_NONCE=7 ROLLUP_OUT="$WORK/o-n7" "$ROLLUP" init >/dev/null 2>&1
grep -q '^NONCE=7$' "$WORK/o-n7/chain-id.env" && grep -q '^CHAIN_ID=4295391545$' "$WORK/o-n7/chain-id.env" && grep -q '"chainId": *4295391545' "$WORK/o-n7/genesis.json" && pass "a payer at nonce 7 gets the id derived from nonce 7 in genesis and record" || fail "nonce 7 renders its own id" "$(cat "$WORK/o-n7/chain-id.env" 2>/dev/null)"

# Named refusals.
refuses() { # $1=label $2=expected name; remaining env assignments are passed in the environment
  local label="$1" name="$2"; shift 2
  if out="$(env "$@" "$ROLLUP" init 2>&1)"; then fail "init refuses: $label" "exited 0"
  elif grep -q "$name" <<<"$out"; then pass "init refuses by name ($name): $label"
  else fail "init refuses by name ($name): $label" "$out"; fi
}
refuses "no .env file" EnvMissing ROLLUP_ENV="$WORK/none.env" ROLLUP_OUT="$WORK/o2"
: > "$WORK/empty.env"; refuses ".env without SOLANA_RPC_URL" SolanaRpcUrlMissing ROLLUP_ENV="$WORK/empty.env" ROLLUP_OUT="$WORK/o3"
refuses "no chain.toml" ChainTomlMissing CHAIN_TOML="$WORK/none.toml" ROLLUP_OUT="$WORK/o4"
{ echo 'chain_id = 200101'; cat "$FIX/chain.toml"; } > "$WORK/bad1.toml"; refuses "a chosen chain_id" ChainIdNotChosen CHAIN_TOML="$WORK/bad1.toml" ROLLUP_OUT="$WORK/o5"
refuses "no payer keypair" PayerKeyMissing PAYER_KEYPAIR_PATH="$WORK/keys/none.json" ROLLUP_OUT="$WORK/o9"
printf '#!/bin/sh\necho "error: rpc down" >&2\nexit 1\n' > "$WORK/failcargo"; chmod +x "$WORK/failcargo"; mkdir -p "$WORK/failbin"; cp "$WORK/failcargo" "$WORK/failbin/cargo"
refuses "the chain-id lookup failing" ChainIdLookupFailed PATH="$WORK/failbin:$PATH" ROLLUP_OUT="$WORK/o10"
refuses "a derived id below 2^32" ChainIdInvalid STUB_ID=1000 ROLLUP_OUT="$WORK/o11"
sed 's/^funded_address = .*/funded_address = "nope"/' "$FIX/chain.toml" > "$WORK/bad2.toml"; refuses "bad funded address" FundedAddressInvalid CHAIN_TOML="$WORK/bad2.toml" ROLLUP_OUT="$WORK/o6"
n=20
for addr in 0x0000000000000000000000000000000000000000 0x0000000000000000000000000000000000000001 0x000000000000000000000000000000000000000A 0x000000000000000000000000000000000000000b 0x00000000000000000000000000000000000000ff 0x4200000000000000000000000000000000000016; do
  n=$((n+1)); sed "s/^funded_address = .*/funded_address = \"$addr\"/" "$FIX/chain.toml" > "$WORK/res$n.toml"
  refuses "funded address $addr" FundedAddressReserved CHAIN_TOML="$WORK/res$n.toml" ROLLUP_OUT="$WORK/o$n"
done
sed 's/^funded_address = .*/funded_address = "0x0000000000000000000000000000000000000100"/' "$FIX/chain.toml" > "$WORK/ok100.toml"
CHAIN_TOML="$WORK/ok100.toml" ROLLUP_OUT="$WORK/o-ok100" "$ROLLUP" init >/dev/null 2>&1 && pass "a funded address just above the precompile range (0x..0100) is accepted" || fail "0x..0100 is accepted" "init refused it"
printf '' > "$WORK/emptyportal.hex"
refuses "an empty exit portal bytecode file" ExitPortalRuntimeMissing EXIT_PORTAL_RUNTIME_HEX="$WORK/emptyportal.hex" ROLLUP_OUT="$WORK/o30"

sed 's/^blocks_per_batch = .*/bogus_knob = 1/' "$FIX/chain.toml" > "$WORK/bad3.toml"; refuses "an unknown profile key" UnknownProfileKey CHAIN_TOML="$WORK/bad3.toml" ROLLUP_OUT="$WORK/o7"
refuses "programs file missing" ProgramsFileMissing PROGRAMS_JSON="$WORK/none.json" ROLLUP_OUT="$WORK/o8"

# Nothing was written into the source tree.
[[ -z "$(git -C "$REPO_ROOT" status --porcelain -- deploy/rollup 2>/dev/null | grep -vE 'tests/|^\?\? deploy/rollup/?$' | grep -E 'rendered|\.toml$|genesis|jwt|compose\.env' || true)" ]] && pass "init wrote only under ROLLUP_OUT" || fail "init wrote only under ROLLUP_OUT" "stray files in deploy/rollup"

finish init_renders_every_config_from_chain_toml
