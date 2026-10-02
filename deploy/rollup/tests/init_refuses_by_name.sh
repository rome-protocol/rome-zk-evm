#!/usr/bin/env bash
# deploy/rollup/tests/init_refuses_by_name.sh — init stops by name when the chain id lookup fails on the RPC, and when
# chain.toml's [profile] sets a key that init sets itself (it would silently override the derived id or a path).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture

if out="$(STUB_CHAIN_ID_FAIL=1 "$ROLLUP" init 2>&1)"; then fail "init refuses when the chain id lookup fails" "exited 0"
elif grep -q '^ChainIdLookupFailed' <<<"$out"; then pass "init refuses by name (ChainIdLookupFailed) when the lookup fails on the RPC"; else fail "ChainIdLookupFailed" "$out"; fi
[[ ! -e "$ROLLUP_OUT/genesis.json" ]] && pass "nothing was rendered after the failed lookup" || fail "nothing was rendered after the failed lookup" "genesis.json exists"

cp "$WORK/chain.toml" "$WORK/chain.toml.orig"
for key in chain_id rpc_addr metrics_addr log_dir sequencer_key_path datadir genesis_path; do
  cp "$WORK/chain.toml.orig" "$WORK/chain.toml"
  python3 - "$WORK/chain.toml" "$key" <<'PY'
import sys
p, key = sys.argv[1], sys.argv[2]
s = open(p).read()
val = "200101" if key == "chain_id" else '"/tmp/x"'
open(p, "w").write(s.replace("[profile]\n", f"[profile]\n{key} = {val}\n", 1))
PY
  : > "$WORK/cargo_calls"
  if out="$("$ROLLUP" init 2>&1)"; then fail "init refuses [profile] $key" "exited 0"
  elif grep -q "^ProfileKeyReserved.*$key" <<<"$out"; then pass "init refuses by name (ProfileKeyReserved) a [profile] $key"; else fail "ProfileKeyReserved for $key" "$out"; fi
  [[ ! -e "$ROLLUP_OUT/genesis.json" ]] || fail "nothing rendered after ProfileKeyReserved ($key)" "genesis.json exists"
done
finish init_refuses_by_name
