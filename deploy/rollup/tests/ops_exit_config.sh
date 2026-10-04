#!/usr/bin/env bash
# deploy/rollup/tests/ops_exit_config.sh — `rollup exit-config propose|activate|show` runs rome-zk-ops exit-config from the
# node image. propose and activate are a dry run until --confirm; show only reads. Every refusal is by name and runs nothing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_exit_config
PAYER="$WORK/keys/payer.json"
PORTAL="0x$(printf '4a%.0s' $(seq 20))"

dry_run_then_confirm "exit-config propose" '^exit-config propose --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538 --chain-authority-keypair /keys/payer --payer-keypair /keys/payer --exit-portal '"$PORTAL"' --exit-cap 500 --activation-delay-slots 1000' \
  exit-config propose --exit-portal "$PORTAL" --exit-cap 500 --activation-delay-slots 1000
keys_mounted_read_only "exit-config propose" "$PAYER"
[[ "$(grep -o 'source=' "$WORK/ops_mounts" | wc -l | tr -d ' ')" == 1 ]] && pass "exit-config propose: the chain authority and the payer are the one key, mounted once" || fail "exit-config propose: one mount" "$(cat "$WORK/ops_mounts")"
no_key_content_anywhere "exit-config propose"

ops_reset_logs; run_ops "exit-config propose (bridge program, bond, a slot)" exit-config propose --bridge-program FixtureEprogram1111111111111111111111111112 --poster-bond 7 --activation-slot 99
grep -q -- '--bridge-program FixtureEprogram1111111111111111111111111112 --poster-bond 7 --activation-slot 99' "$WORK/ops_calls" && pass "exit-config propose: every field and the activation slot reach rome-zk-ops" || fail "exit-config propose: fields" "$(cat "$WORK/ops_calls")"

dry_run_then_confirm "exit-config activate" '^exit-config activate --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538 --payer-keypair /keys/payer' exit-config activate
keys_mounted_read_only "exit-config activate" "$PAYER"
no_key_content_anywhere "exit-config activate"

ops_reset_logs; run_ops "exit-config show" exit-config show
grep -qE '^exit-config show --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538$' "$WORK/ops_calls" && pass "exit-config show: a read, with no key and no --confirm" || fail "exit-config show: a read" "$(cat "$WORK/ops_calls")"
grep -q 'source=' "$WORK/ops_mounts" && fail "exit-config show: mounts no key" "$(cat "$WORK/ops_mounts")" || pass "exit-config show: mounts no key"
grep -q 'exit_portal' <<<"$OUT_TEXT" && pass "exit-config show: prints what rome-zk-ops answered" || fail "exit-config show: prints the answer" "$OUT_TEXT"

R=("$ROLLUP" exit-config propose)
refuses_by_name NothingProposed "${R[@]}" --activation-delay-slots 10
refuses_by_name ActivationMissing "${R[@]}" --exit-cap 5
refuses_by_name ActivationConflict "${R[@]}" --exit-cap 5 --activation-slot 9 --activation-delay-slots 10
refuses_by_name FlagValueMissing "${R[@]}" --exit-cap
refuses_by_name FlagValueInvalid "${R[@]}" --exit-cap many --activation-slot 9
refuses_by_name FlagValueInvalid "${R[@]}" --poster-bond 1.5 --activation-slot 9
refuses_by_name FlagValueInvalid "${R[@]}" --activation-slot soon --exit-cap 5
refuses_by_name FlagValueInvalid "${R[@]}" --exit-portal 0x1234 --activation-slot 9
refuses_by_name FlagValueInvalid "${R[@]}" --exit-portal 0xzz4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a --activation-slot 9
refuses_by_name UnknownArgument "${R[@]}" --colour blue
refuses_by_name UnknownArgument "$ROLLUP" exit-config activate --activation-slot 9
refuses_by_name UnknownArgument "$ROLLUP" exit-config show --confirm
refuses_by_name UnknownSubcommand "$ROLLUP" exit-config
refuses_by_name UnknownSubcommand "$ROLLUP" exit-config remove
finish ops_exit_config
