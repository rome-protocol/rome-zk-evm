#!/usr/bin/env bash
# deploy/rollup/tests/ops_refund_deposit.sh — `rollup refund-deposit` runs rome-zk-ops refund-deposit from the node image.
# It is a dry run until --confirm, mounts the payer key read-only by its path, and never prints a key.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_refund_deposit
PAYER="$WORK/keys/payer.json"

dry_run_then_confirm "refund-deposit" '^refund-deposit --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538 --keypair /keys/payer' refund-deposit
keys_mounted_read_only "refund-deposit" "$PAYER"
no_key_content_anywhere "refund-deposit"

refuses_by_name UnknownArgument "$ROLLUP" refund-deposit now
refuses_by_name UnknownArgument "$ROLLUP" refund-deposit --amount 5
finish ops_refund_deposit
