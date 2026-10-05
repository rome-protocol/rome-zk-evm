#!/usr/bin/env bash
# deploy/rollup/tests/exit_init_and_up.sh — EXITS=on turns withdrawals on. `init` then renders the exit prover's config
# from what it already knows, and `up` starts the exits profile; without EXITS=on init renders no exit config and
# compose gets no exits profile. The portal is never configured: the exit prover reads it from the chain.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
printf 'ab%.0s' $(seq 32) > "$WORK/keys/sequencer.key"

# Off: nothing rendered, no profile.
"$ROLLUP" init >/dev/null 2>&1
[[ ! -e "$ROLLUP_OUT/exit-prover.toml" ]] && pass "no exit-prover.toml when EXITS is not on" || fail "no exit-prover.toml when EXITS is not on" "found one"
: > "$WORK/docker_calls"; "$ROLLUP" up >/dev/null 2>&1
grep -q -- '--profile exits' "$WORK/docker_calls" && fail "up without EXITS=on has no exits profile" "$(cat "$WORK/docker_calls")" || pass "up without EXITS=on has no exits profile"

# On, without a payer of its own: refused by name, nothing rendered for the exit prover.
echo "EXITS=on" >> "$ROLLUP_ENV"
EXIT_KEY="$WORK/keys/exit-payer.json"
echo "EXIT_PAYER_KEYPAIR_PATH=$EXIT_KEY" >> "$ROLLUP_ENV"
out="$("$ROLLUP" init 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^ExitPayerMissing: ' <<<"$out" && pass "EXITS=on without an exit payer file is refused as ExitPayerMissing" || fail "ExitPayerMissing" "exit $code: $out"
grep -q 'solana-keygen new' <<<"$out" && pass "the refusal says how to make the key" || fail "the refusal says how to make the key" "$out"
[[ ! -e "$ROLLUP_OUT/exit-prover.toml" ]] && pass "a refused init renders no exit config" || fail "a refused init renders no exit config" "found one"

# On, with the batcher's own key (the same public key): refused by name.
echo '[4,5,6]' > "$EXIT_KEY"; echo "SamePublicKey11111111111111111111111111111" > "$EXIT_KEY.pub"; echo "SamePublicKey11111111111111111111111111111" > "$WORK/keys/payer.json.pub"
out="$("$ROLLUP" init 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^ExitPayerIsBatcherPayer: ' <<<"$out" && pass "an exit payer with the batcher payer's public key is refused as ExitPayerIsBatcherPayer" || fail "ExitPayerIsBatcherPayer" "exit $code: $out"
[[ ! -e "$ROLLUP_OUT/exit-prover.toml" ]] && pass "a refused init renders no exit config (same key)" || fail "a refused init renders no exit config (same key)" "found one"
rm -f "$WORK/keys/payer.json.pub"

# On, when reading the exit payer's public key fails (an RPC error during init): refused, never rendered without the
# check, so the batcher's key can never reach the exit prover unnoticed.
touch "$EXIT_KEY.fail"
out="$("$ROLLUP" init 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q "^ChainIdLookupFailed: " <<<"$out" && pass "a failed read of the exit payer's public key stops init by name" || fail "a failed exit payer read stops init" "exit $code: $out"
[[ ! -e "$ROLLUP_OUT/exit-prover.toml" ]] && pass "a failed exit payer read renders no exit config" || fail "a failed exit payer read renders no exit config" "found one"
rm -f "$EXIT_KEY.fail"

# On, with a payer of its own.
echo "ExitPayerStub1111111111111111111111111111111" > "$EXIT_KEY.pub"
if out="$("$ROLLUP" init 2>&1)"; then pass "EXITS=on renders"; else fail "EXITS=on renders" "$out"; fi
grep -q '^EXIT_PAYER_KEYPAIR_PATH='"$EXIT_KEY"'$' "$ROLLUP_OUT/compose.env" && pass "compose.env carries the exit payer's path" || fail "compose.env EXIT_PAYER_KEYPAIR_PATH" "$(grep KEYPAIR "$ROLLUP_OUT/compose.env")"
grep -q '^EXIT_PAYER=ExitPayerStub1111111111111111111111111111111$' "$ROLLUP_OUT/chain-id.env" && pass "chain-id.env records the exit payer's public key for check" || fail "chain-id.env EXIT_PAYER" "$(cat "$ROLLUP_OUT/chain-id.env")"
grep -q "source=$EXIT_KEY," "$WORK/ops_mounts" && pass "the exit key is read through the same read-only mount as the payer key" || fail "exit key mount" "$(tail -3 "$WORK/ops_mounts")"
T="$ROLLUP_OUT/exit-prover.toml"
[[ -f "$T" ]] && pass "init renders exit-prover.toml" || { fail "init renders exit-prover.toml" "missing"; finish exit_init_and_up; }
grep -qE '^chain_id = 4295391538$' "$T" && pass "it carries the chain id init derived" || fail "chain_id" "$(head -c 400 "$T")"
grep -qE '^settlement_program_id = "FixtureBprogram11111111111111111111111111112"$' "$T" && pass "it carries the programs file's settlement id" || fail "settlement_program_id" "$(grep settlement "$T")"
grep -qE '^settlement_rpc = "https://rpc.example.invalid"$' "$T" && pass "it reads Solana through SOLANA_RPC_URL" || fail "settlement_rpc" "$(grep rpc "$T")"
grep -qE '^verifier_rpc = "http://reth-verifier:8547"$' "$T" && pass "it reads the L2 from the verifier node on the compose network" || fail "verifier_rpc" "$(grep rpc "$T")"
grep -qE '^payer_key_path = "/run/rome-zk/payer.json"$' "$T" && pass "the payer key path is where compose mounts it" || fail "payer_key_path" "$(grep payer "$T")"
grep -qE '^metrics_addr = "0.0.0.0:9005"$' "$T" && pass "metrics are served on 9005 inside the container" || fail "metrics_addr" "$(grep metrics "$T")"
grep -qiE 'portal' <(grep -v '^#' "$T") && fail "the portal is not configured" "$(grep -i portal "$T")" || pass "the portal address is not in the config (it comes from the chain's exit config)"
grep -qE '__[A-Z0-9_]+__' "$T" && fail "no placeholder is left" "$(grep -E '__[A-Z0-9_]+__' "$T")" || pass "no placeholder is left"
grep -q '^EXIT_PROVER_METRICS_PORT=9005$' "$ROLLUP_OUT/compose.env" && pass "compose.env carries EXIT_PROVER_METRICS_PORT" || fail "compose.env EXIT_PROVER_METRICS_PORT" "$(grep METRICS "$ROLLUP_OUT/compose.env")"

# The binary accepts what init rendered: the config keys are the ones the exit prover knows (its parser refuses others).
known="$(sed -n 's/^ *pub \([a-z_]*\): .*/\1/p' "$REPO_ROOT/crates/rome-zk-exit-prover/src/config.rs" | sort -u)"
bad=""; for k in $(grep -E '^[a-z_]+ *=' "$T" | sed 's/ *=.*//'); do grep -qx "$k" <<<"$known" || bad="$bad $k"; done
[[ -z "$bad" ]] && pass "every key in exit-prover.toml is a field of the exit prover's config" || fail "unknown keys in exit-prover.toml" "$bad"

# up starts the profile.
: > "$WORK/docker_calls"
if out="$("$ROLLUP" up 2>&1)"; then pass "up with EXITS=on succeeds"; else fail "up with EXITS=on" "$out"; fi
grep -q -- '--profile exits' "$WORK/docker_calls" && pass "up passes --profile exits to compose" || fail "up passes --profile exits" "$(cat "$WORK/docker_calls")"
: > "$WORK/docker_calls"; "$ROLLUP" down >/dev/null 2>&1
grep -q -- '--profile exits' "$WORK/docker_calls" && pass "down and logs see the profile too" || fail "down sees the profile" "$(cat "$WORK/docker_calls")"
if out="$("$ROLLUP" logs exit-prover 2>&1)"; then pass "logs knows the exit-prover service"; else fail "logs knows exit-prover" "$out"; fi

# up refuses to start the exit prover without its own key, and never needs the batcher's key for it.
mv "$EXIT_KEY" "$EXIT_KEY.gone"; : > "$WORK/docker_calls"
out="$("$ROLLUP" up exit-prover 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^ExitPayerMissing: ' <<<"$out" && pass "up exit-prover refuses a missing exit payer by name" || fail "up ExitPayerMissing" "exit $code: $out"
[[ ! -s "$WORK/docker_calls" ]] && pass "and starts nothing" || fail "and starts nothing" "$(cat "$WORK/docker_calls")"
out="$("$ROLLUP" up 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^ExitPayerMissing: ' <<<"$out" && pass "up with no service named refuses it too" || fail "up (all) ExitPayerMissing" "exit $code: $out"
mv "$EXIT_KEY.gone" "$EXIT_KEY"
mv "$WORK/keys/payer.json" "$WORK/keys/payer.json.gone"
if out="$("$ROLLUP" up exit-prover 2>&1)"; then pass "the exit prover starts without the batcher's payer key being needed"; else fail "exit-prover needs no batcher payer key" "$out"; fi
mv "$WORK/keys/payer.json.gone" "$WORK/keys/payer.json"

# Both profiles together.
echo "PROVER=on" >> "$ROLLUP_ENV"; echo '{"elf_sha256":"0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"}' > "$WORK/vkey.json"; echo "VKEY_JSON=$WORK/vkey.json" >> "$ROLLUP_ENV"
"$ROLLUP" init >/dev/null 2>&1; : > "$WORK/docker_calls"; "$ROLLUP" up >/dev/null 2>&1
grep -q -- '--profile prover --profile exits' "$WORK/docker_calls" && pass "PROVER=on and EXITS=on start both profiles" || fail "both profiles" "$(cat "$WORK/docker_calls")"

# Turning it off again removes the stale config.
grep -v '^EXITS=on$' "$ROLLUP_ENV" > "$ROLLUP_ENV.new" && mv "$ROLLUP_ENV.new" "$ROLLUP_ENV"
"$ROLLUP" init >/dev/null 2>&1
[[ ! -e "$T" ]] && pass "turning EXITS off removes the rendered exit config" || fail "turning EXITS off removes the rendered exit config" "still there"
finish exit_init_and_up
