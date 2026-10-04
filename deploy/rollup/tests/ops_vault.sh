#!/usr/bin/env bash
# deploy/rollup/tests/ops_vault.sh — `rollup vault init|fund|show` runs rome-zk-ops vault from the node image with the bridge
# program from the programs file. init and fund are a dry run until --confirm; show only reads. Refusals are by name.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_vault
PAYER="$WORK/keys/payer.json"
COMMON='--settlement FixtureBprogram11111111111111111111111111112 --bridge FixtureDprogram11111111111111111111111111112 --chain-id 4295391538'

dry_run_then_confirm "vault init" "^vault init --authority-keypair /keys/payer --mint So11111111111111111111111111111111111111112 --mint-decimals 9 $COMMON" vault init
keys_mounted_read_only "vault init" "$PAYER"
no_key_content_anywhere "vault init"
ops_reset_logs; run_ops "vault init (another mint)" vault init --mint FixtureMint11111111111111111111111111111112 --mint-decimals 6
grep -q -- '--mint FixtureMint11111111111111111111111111111112 --mint-decimals 6' "$WORK/ops_calls" && pass "vault init: the mint and its decimals reach rome-zk-ops" || fail "vault init: the mint" "$(cat "$WORK/ops_calls")"

dry_run_then_confirm "vault fund" "^vault fund --payer-keypair /keys/payer --amount 123456 $COMMON" vault fund --amount 123456
keys_mounted_read_only "vault fund" "$PAYER"
no_key_content_anywhere "vault fund"

ops_reset_logs; run_ops "vault show" vault show
grep -qE "^vault show $COMMON\$" "$WORK/ops_calls" && pass "vault show: a read, with no key and no --confirm" || fail "vault show: a read" "$(cat "$WORK/ops_calls")"
grep -q 'source=' "$WORK/ops_mounts" && fail "vault show: mounts no key" "$(cat "$WORK/ops_mounts")" || pass "vault show: mounts no key"

refuses_by_name AmountMissing "$ROLLUP" vault fund
refuses_by_name FlagValueMissing "$ROLLUP" vault fund --amount
refuses_by_name FlagValueInvalid "$ROLLUP" vault fund --amount lots
refuses_by_name FlagValueInvalid "$ROLLUP" vault fund --amount -5
refuses_by_name FlagValueInvalid "$ROLLUP" vault init --mint-decimals nine
refuses_by_name FlagValueMissing "$ROLLUP" vault init --mint
refuses_by_name UnknownArgument "$ROLLUP" vault init --colour blue
refuses_by_name UnknownArgument "$ROLLUP" vault fund --amount 5 --to me
refuses_by_name UnknownArgument "$ROLLUP" vault show --confirm
refuses_by_name UnknownSubcommand "$ROLLUP" vault
refuses_by_name UnknownSubcommand "$ROLLUP" vault drain
# The vault needs the bridge program: a programs file without it is refused by name before anything runs.
jq 'del(.programs["zk-bridge"])' "$FIX/programs.json" > "$WORK/nobridge.json"
for c in "vault init" "vault fund --amount 5" "vault show"; do
  # shellcheck disable=SC2086
  refuses_by_name ProgramsFileInvalid env PROGRAMS_JSON="$WORK/nobridge.json" "$ROLLUP" $c
done
finish ops_vault
