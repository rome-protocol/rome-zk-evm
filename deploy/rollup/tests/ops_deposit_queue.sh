#!/usr/bin/env bash
# deploy/rollup/tests/ops_deposit_queue.sh — `rollup deposit-queue init|propose|activate|show`, `rollup deposit` and
# `rollup close-deposit` run rome-zk-ops from the node image with the bridge program from the programs file. Everything
# that sends is a dry run until --confirm; show only reads. `init` and `propose` pass the blocks per batch the chain was
# rendered with. Every refusal is by name and runs nothing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_deposit_queue
PAYER="$WORK/keys/payer.json"
DEPOSITOR="$WORK/keys/depositor.json"; echo "$KEY_MARK" > "$DEPOSITOR"
COMMON='--settlement FixtureBprogram11111111111111111111111111112 --bridge FixtureDprogram11111111111111111111111111112 --chain-id 4295391538'
RECIPIENT="0x$(printf '7c%.0s' $(seq 20))"

# ---- deposit-queue init ---------------------------------------------------------------------------------------------------
dry_run_then_confirm "deposit-queue init" "^deposit-queue init --authority-keypair /keys/payer $COMMON --blocks-per-batch 30" deposit-queue init
keys_mounted_read_only "deposit-queue init" "$PAYER"
no_key_content_anywhere "deposit-queue init"
ops_reset_logs; run_ops "deposit-queue init (every parameter)" deposit-queue init --deadline-secs 7200 --max-per-batch 128 --max-per-block 2 --min-amount 5000 --fee-lamports 7 --fee-recipient Fixturepayer11111111111111111111111111111112
grep -q -- '--deadline-secs 7200 --max-per-batch 128 --max-per-block 2 --min-amount 5000 --fee-lamports 7 --fee-recipient Fixturepayer11111111111111111111111111111112 --blocks-per-batch 30' "$WORK/ops_calls" \
  && pass "deposit-queue init: every parameter reaches rome-zk-ops, with the rendered blocks per batch" || fail "deposit-queue init: parameters" "$(cat "$WORK/ops_calls")"
ops_reset_logs; run_ops "deposit-queue init (none given)" deposit-queue init
grep -q -- '--deadline-secs\|--max-per-block\|--fee-lamports' "$WORK/ops_calls" && fail "deposit-queue init: the defaults are rome-zk-ops's own" "$(cat "$WORK/ops_calls")" || pass "deposit-queue init: with no parameter given the ruled defaults are left to rome-zk-ops"
# blocks_per_batch follows the rendered config, not a constant.
cp "$WORK/out/compose.env" "$WORK/compose.env.orig"
sed -i.bak 's/^BLOCKS_PER_BATCH=.*/BLOCKS_PER_BATCH=1_200/' "$WORK/out/compose.env"
ops_reset_logs; run_ops "deposit-queue init (another blocks_per_batch)" deposit-queue init
grep -q -- '--blocks-per-batch 1200$' "$WORK/ops_calls" && pass "deposit-queue init: --blocks-per-batch is read from the rendered config" || fail "deposit-queue init: blocks per batch" "$(cat "$WORK/ops_calls")"
sed -i.bak 's/^BLOCKS_PER_BATCH=.*/BLOCKS_PER_BATCH=none/' "$WORK/out/compose.env"
refuses_by_name NotInitialised "$ROLLUP" deposit-queue init
refuses_by_name NotInitialised "$ROLLUP" deposit-queue propose --max-per-block 3
cp "$WORK/compose.env.orig" "$WORK/out/compose.env"

# ---- deposit-queue propose ------------------------------------------------------------------------------------------------
dry_run_then_confirm "deposit-queue propose" "^deposit-queue propose --authority-keypair /keys/payer $COMMON --max-per-block 3 --activation-delay-slots 900 --blocks-per-batch 30" \
  deposit-queue propose --max-per-block 3 --activation-delay-slots 900
keys_mounted_read_only "deposit-queue propose" "$PAYER"
no_key_content_anywhere "deposit-queue propose"
ops_reset_logs; run_ops "deposit-queue propose (a slot, several fields)" deposit-queue propose --fee-lamports 9 --min-amount 2000000 --activation-slot 777
grep -q -- '--fee-lamports 9 --min-amount 2000000 --activation-slot 777 --blocks-per-batch 30' "$WORK/ops_calls" && pass "deposit-queue propose: the fields and the activation slot reach rome-zk-ops" || fail "deposit-queue propose: fields" "$(cat "$WORK/ops_calls")"
ops_reset_logs; run_ops "deposit-queue propose (no activation given)" deposit-queue propose --deadline-secs 3600
grep -q -- '--activation' "$WORK/ops_calls" && fail "deposit-queue propose: the default activation is rome-zk-ops's" "$(cat "$WORK/ops_calls")" || pass "deposit-queue propose: with no activation given the default is left to rome-zk-ops"

# ---- deposit-queue activate and show ---------------------------------------------------------------------------------------
dry_run_then_confirm "deposit-queue activate" "^deposit-queue activate --payer-keypair /keys/payer $COMMON" deposit-queue activate
keys_mounted_read_only "deposit-queue activate" "$PAYER"
no_key_content_anywhere "deposit-queue activate"
ops_reset_logs; run_ops "deposit-queue show" deposit-queue show
grep -qE "^deposit-queue show $COMMON\$" "$WORK/ops_calls" && pass "deposit-queue show: a read, with no key and no --confirm" || fail "deposit-queue show: a read" "$(cat "$WORK/ops_calls")"
grep -q 'source=' "$WORK/ops_mounts" && fail "deposit-queue show: mounts no key" "$(cat "$WORK/ops_mounts")" || pass "deposit-queue show: mounts no key"

# ---- deposit ------------------------------------------------------------------------------------------------------------------
dry_run_then_confirm "deposit" "^deposit $COMMON --amount 2500000 --recipient $RECIPIENT --keypair /keys/depositor" deposit --amount 2500000 --recipient "$RECIPIENT" --keypair "$DEPOSITOR"
keys_mounted_read_only "deposit" "$DEPOSITOR"
grep -q "source=$PAYER" "$WORK/ops_mounts" && fail "deposit: the chain authority's key is not mounted" "$(cat "$WORK/ops_mounts")" || pass "deposit: only the depositor's key is mounted"
no_key_content_anywhere "deposit"
ops_reset_logs; run_ops "deposit (wrap SOL)" deposit --amount 2500000 --recipient "$RECIPIENT" --keypair "$DEPOSITOR" --wrap-sol
grep -q -- '--wrap-sol --keypair /keys/depositor' "$WORK/ops_calls" && pass "deposit: --wrap-sol reaches rome-zk-ops" || fail "deposit: --wrap-sol" "$(cat "$WORK/ops_calls")"

# ---- close-deposit -------------------------------------------------------------------------------------------------------------
dry_run_then_confirm "close-deposit" "^close-deposit $COMMON --index 12 --payer-keypair /keys/payer" close-deposit --index 12
keys_mounted_read_only "close-deposit" "$PAYER"
no_key_content_anywhere "close-deposit"

# ---- refusals, by name, before anything runs ------------------------------------------------------------------------------------
Q="$ROLLUP deposit-queue"
refuses_by_name FlagValueMissing "$ROLLUP" deposit-queue init --max-per-block
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue init --max-per-block many
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue init --deadline-secs -5
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue init --fee-lamports 1.5
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue init --fee-recipient not-an-address
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue propose --fee-recipient 0x1234 --activation-slot 5
refuses_by_name FlagValueInvalid "$ROLLUP" deposit-queue propose --max-per-block 3 --activation-slot soon
refuses_by_name FlagValueMissing "$ROLLUP" deposit-queue propose --max-per-block 3 --activation-delay-slots
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue init --colour blue
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue init --blocks-per-batch 9
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue init --activation-slot 9
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue propose --max-per-block 3 --bogus
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue activate --activation-slot 9
refuses_by_name UnknownArgument "$ROLLUP" deposit-queue show --confirm
refuses_by_name NothingProposed "$ROLLUP" deposit-queue propose --activation-delay-slots 10
refuses_by_name NothingProposed "$ROLLUP" deposit-queue propose
refuses_by_name ActivationConflict "$ROLLUP" deposit-queue propose --max-per-block 3 --activation-slot 9 --activation-delay-slots 10
refuses_by_name UnknownSubcommand "$ROLLUP" deposit-queue
refuses_by_name UnknownSubcommand "$ROLLUP" deposit-queue drain

refuses_by_name AmountMissing "$ROLLUP" deposit --recipient "$RECIPIENT" --keypair "$DEPOSITOR"
refuses_by_name RecipientMissing "$ROLLUP" deposit --amount 5 --keypair "$DEPOSITOR"
refuses_by_name KeypairMissing "$ROLLUP" deposit --amount 5 --recipient "$RECIPIENT"
refuses_by_name FlagValueInvalid "$ROLLUP" deposit --amount lots --recipient "$RECIPIENT" --keypair "$DEPOSITOR"
refuses_by_name FlagValueInvalid "$ROLLUP" deposit --amount 5 --recipient 0x1234 --keypair "$DEPOSITOR"
refuses_by_name FlagValueInvalid "$ROLLUP" deposit --amount 5 --recipient 0xzz7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c --keypair "$DEPOSITOR"
refuses_by_name FlagValueMissing "$ROLLUP" deposit --amount
refuses_by_name UnknownArgument "$ROLLUP" deposit --amount 5 --recipient "$RECIPIENT" --keypair "$DEPOSITOR" --to me
refuses_by_name KeyFileMissing "$ROLLUP" deposit --amount 5 --recipient "$RECIPIENT" --keypair "$WORK/keys/nobody.json"

refuses_by_name IndexMissing "$ROLLUP" close-deposit
refuses_by_name FlagValueInvalid "$ROLLUP" close-deposit --index first
refuses_by_name FlagValueInvalid "$ROLLUP" close-deposit --index -1
refuses_by_name FlagValueMissing "$ROLLUP" close-deposit --index
refuses_by_name UnknownArgument "$ROLLUP" close-deposit --index 1 --all

# The bridge program is needed by every one of them: a programs file without it is refused by name before anything runs.
jq 'del(.programs["zk-bridge"])' "$FIX/programs.json" > "$WORK/nobridge.json"
for c in "deposit-queue init" "deposit-queue propose --max-per-block 3" "deposit-queue activate" "deposit-queue show" \
         "deposit --amount 5 --recipient $RECIPIENT --keypair $DEPOSITOR" "close-deposit --index 1"; do
  # shellcheck disable=SC2086
  refuses_by_name ProgramsFileInvalid env PROGRAMS_JSON="$WORK/nobridge.json" "$ROLLUP" $c
done
finish ops_deposit_queue
