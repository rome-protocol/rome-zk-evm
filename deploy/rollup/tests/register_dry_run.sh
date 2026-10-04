#!/usr/bin/env bash
# deploy/rollup/tests/register_dry_run.sh — `rollup register` wraps rome-zk-ops register (permissionless). It refuses to do
# anything given neither or both of --dry-run / --confirm, and --dry-run prints every command without running docker
# (the node image's rome-zk-ops), calling the chain or reading a key.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
"$ROLLUP" init >/dev/null 2>&1
mkdir -p "$WORK/bin"
for tool in docker curl solana; do printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\nexit 1\n' "$tool" "$WORK" > "$WORK/bin/$tool"; chmod +x "$WORK/bin/$tool"; done
export PATH="$WORK/bin:$PATH"

if out="$("$ROLLUP" register 2>&1)"; then fail "register with no flag refuses" "exited 0"
elif grep -q 'refusing' <<<"$out"; then pass "register with neither --dry-run nor --confirm refuses"; else fail "register with no flag refuses" "$out"; fi
if out="$("$ROLLUP" register --dry-run --confirm 2>&1)"; then fail "register with both flags refuses" "exited 0"
elif grep -q 'mutually exclusive' <<<"$out"; then pass "register with both flags refuses"; else fail "register with both flags refuses" "$out"; fi
[[ ! -e "$WORK/calls" ]] && pass "the refusals called no tool" || fail "the refusals called no tool" "$(cat "$WORK/calls")"

if out="$("$ROLLUP" register --dry-run 2>&1)"; then pass "register --dry-run succeeds"; else fail "register --dry-run succeeds" "$out"; fi
grep -q -- '--permissionless' <<<"$out" && pass "the dry run shows the permissionless registration" || fail "the dry run shows --permissionless" "$out"
grep -q 'FixtureBprogram11111111111111111111111111112' <<<"$out" && pass "the dry run names the settlement program from the programs file" || fail "settlement id in the dry run" "$out"
grep -q 'FixtureAprogram11111111111111111111111111112' <<<"$out" && pass "the dry run names the inbox program from the programs file" || fail "inbox id in the dry run" "$out"
grep -q -- '--rpc-url https://rpc.example.invalid' <<<"$out" && pass "the dry run uses the Solana RPC from .env" || fail "rpc url in the dry run" "$out"
grep -qi 'vkey' <<<"$out" && fail "the dry run carries no verifier-key flag" "$(grep -i vkey <<<"$out")" || pass "the dry run carries no verifier-key flag (a permissionless chain registers with none)"
grep -q -- 'chain-id --settlement' <<<"$out" && pass "the dry run shows the chain-id re-read that must still match" || fail "chain-id re-read in the dry run" "$out"
grep -q 'authority=StubAuthority1111111111111111111111111111111 nonce=0' <<<"$out" && pass "the dry run states the authority and nonce init recorded" || fail "authority and nonce in the dry run" "$out"
grep -q -- '--nonce 0 --expect-chain-id 4295391538' <<<"$out" && pass "the dry run sends init's recorded nonce and chain id" || fail "nonce and chain id flags in the dry run" "$out"
grep -q -- '--next-batch 1' <<<"$out" && pass "the dry run initialises the batch cursor at 1" || fail "init_cursor in the dry run" "$out"
grep -q '4295391538' <<<"$out" && pass "the dry run states the chain id the genesis was rendered with" || fail "chain id in the dry run" "$out"
[[ ! -e "$WORK/calls" ]] && pass "--dry-run called no docker, curl or solana" || fail "--dry-run called no tool" "$(cat "$WORK/calls")"

rm -rf "$ROLLUP_OUT"
if out="$("$ROLLUP" register --dry-run 2>&1)"; then fail "register before init refuses" "exited 0"
elif grep -q 'NotInitialised' <<<"$out"; then pass "register before init refuses by name (NotInitialised)"; else fail "register before init refuses by name" "$out"; fi
finish register_dry_run
