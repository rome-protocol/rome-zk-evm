#!/usr/bin/env bash
# deploy/rollup/tests/init_prover_optional.sh — the prover is opt-in (one GPU). Without PROVER=on, init renders no
# prover config and asks for nothing prover-related; with it, init renders prover.toml from the same chain.toml and
# programs file, plus a database password generated once, and refuses by name when the prover's inputs are missing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture

"$ROLLUP" init >/dev/null 2>&1
[[ ! -e "$ROLLUP_OUT/prover.toml" ]] && pass "no prover.toml when PROVER is not on" || fail "no prover.toml when PROVER is not on" "found one"

echo "PROVER=on" >> "$ROLLUP_ENV"
if out="$("$ROLLUP" init 2>&1)"; then fail "PROVER=on without a vkey file refuses" "exited 0"
elif grep -q 'ProverVkeyMissing' <<<"$out"; then pass "PROVER=on without VKEY_JSON refuses by name (ProverVkeyMissing)"
else fail "PROVER=on without VKEY_JSON refuses by name" "$out"; fi

echo '{"elf_sha256":"0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"}' > "$WORK/vkey.json"
echo "VKEY_JSON=$WORK/vkey.json" >> "$ROLLUP_ENV"
if out="$("$ROLLUP" init 2>&1)"; then pass "PROVER=on with a vkey file renders"; else fail "PROVER=on with a vkey file renders" "$out"; fi
P="$ROLLUP_OUT/prover.toml"
grep -qE '^chain_id = 4295391538$' "$P" && pass "prover.toml carries chain_id from chain.toml" || fail "prover.toml chain_id" "$(head -c 300 "$P")"
grep -qE '^inbox_program_id = "FixtureAprogram11111111111111111111111111112"$' "$P" && pass "prover.toml carries the programs file's inbox id" || fail "prover.toml inbox id" "missing"
grep -qE '^verifier_rpc_url = "http://reth-verifier:8547"$' "$P" && pass "prover reads the private verifier on the compose network" || fail "prover verifier url" "missing"
grep -qE '^elf_path = "/data/elf/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789.elf"$' "$P" && pass "elf_path is pinned by the vkey file's elf_sha256" || fail "elf_path" "$(grep elf_path "$P")"
grep -qE '^database_url = "postgres://rome:[0-9a-f]{32,}@postgres:5432/prover"$' "$P" && pass "history database is the local postgres container" || fail "database_url" "$(grep database_url "$P" | sed 's/:[0-9a-f]\{32,\}@/:<hidden>@/')"
pw1="$(grep database_url "$P")"; "$ROLLUP" init >/dev/null 2>&1; pw2="$(grep database_url "$P")"
[[ "$pw1" == "$pw2" ]] && pass "the database password is generated once" || fail "the database password is generated once" "changed on re-run"
if grep -rq 'gpu = true' "$P"; then pass "prover.toml enables the GPU"; else fail "prover.toml enables the GPU" "gpu = true missing"; fi
finish init_prover_optional
