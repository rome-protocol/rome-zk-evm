#!/usr/bin/env bash
# deploy/rollup/tests/ops_release_exit.sh — `rollup release-exit` runs rome-zk-ops release-exit from the node image. It is a
# dry run until --confirm, mounts the payer key read-only by its path and never prints it.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_release_exit
PAYER="$WORK/keys/payer.json"
HASH="0x$(printf 'c3%.0s' $(seq 32))"

dry_run_then_confirm "release-exit" "^release-exit --settlement FixtureBprogram11111111111111111111111111112 --bridge FixtureDprogram11111111111111111111111111112 --chain-id 4295391538 --message-hash $HASH --payer-keypair /keys/payer" release-exit --message-hash "$HASH"
keys_mounted_read_only "release-exit" "$PAYER"
no_key_content_anywhere "release-exit"

refuses_by_name MessageHashMissing "$ROLLUP" release-exit
refuses_by_name FlagValueMissing "$ROLLUP" release-exit --message-hash
refuses_by_name FlagValueInvalid "$ROLLUP" release-exit --message-hash 0x1234
refuses_by_name FlagValueInvalid "$ROLLUP" release-exit --message-hash "0x$(printf 'zz%.0s' $(seq 32))"
refuses_by_name FlagValueInvalid "$ROLLUP" release-exit --message-hash "0x$(printf 'c3%.0s' $(seq 33))"
refuses_by_name UnknownArgument "$ROLLUP" release-exit --message-hash "$HASH" --to me
jq 'del(.programs["zk-bridge"])' "$FIX/programs.json" > "$WORK/nobridge.json"
refuses_by_name ProgramsFileInvalid env PROGRAMS_JSON="$WORK/nobridge.json" "$ROLLUP" release-exit --message-hash "$HASH"
finish ops_release_exit
