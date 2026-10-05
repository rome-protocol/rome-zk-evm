#!/usr/bin/env bash
# deploy/rollup/tests/check_deposit_items.sh — the `rollup check` items about deposits: the deposit-capable key, the caps,
# the genesis balance and the vault, the exit config's bridge, the sequencer's [deposits] section, the batch cursor, the
# oldest waiting deposit and the backlog. Each has a passing, a failing and a skipped case, and a failure names what is
# wrong and the fix. docker (rome-zk-ops) and curl are stubs; every answer comes from a file under $S.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
prepare_check check_deposit_items

run() { echo 100 > "$S/seq_block"; echo 95 > "$S/ver_block"; "$ROLLUP" check 2>&1; }   # the stub's blocks advance on every read: start each run level
expect() { # $1=output $2=pattern $3=label
  grep -qE -- "$2" <<<"$1" && pass "$3" || fail "$3" "no line /$2/ in: $(grep -E '^(PASS|FAIL|SKIP|WARN)' <<<"$1" | head -30 | tr '\n' '|')"
}
BRIDGE=FixtureDprogram11111111111111111111111111112
PVK=0x$(printf 'ab%.0s' $(seq 32))
OTHER=0x$(printf 'cd%.0s' $(seq 32))
GUEST="$ROLLUP_OUT/guest"; mkdir -p "$GUEST"
vkey_json() { # $1=programVK or none, $2=guest tag or none
  local src='built by rollup guest-build from rome-zk-evm v0.2.1 and rome-zk-guest '"$2"
  [[ "$2" != none ]] || src="written by hand"
  if [[ "$1" == none ]]; then jq -n --arg s "$src" '{elf_sha256: "x", source: $s}' > "$GUEST/vkey.json"
  else jq -n --arg p "$1" --arg s "$src" '{programVK: $p, elf_sha256: "x", source: $s}' > "$GUEST/vkey.json"; fi
}
registry() { # $@=entry lines
  { echo "chain_id=4295391538"; echo "slot=1000"; echo "vkey_entries=$#"; echo "vkey_active=yes"; printf '%s\n' "$@"; } > "$S/vkey_show"
}
entry() { echo "vkey_entry_$1=state=$2 layout=1 curve=0 scheme=1 vkey=$3 activation_slot=$4"; }
queue() { # $1=max_per_block $2=max_per_batch, then extra lines
  local pb="$1" pbatch="$2" pend=false a; shift 2
  for a in "$@"; do [[ "$a" != pending=true ]] || pend=true; done
  { echo "chain_id=4295391538"; echo "deposit_queue=StubQueue"; echo "queue_exists=true"; echo "count=10"
    echo "inclusion_deadline_secs=43200"; echo "max_per_batch=$pbatch"; echo "max_per_block=$pb"; echo "min_amount=1000000"
    echo "fee_lamports=100000"; echo "fee_recipient=StubFee"; echo "slot=1000"; echo "pending=$pend"
    printf '%s\n' "$@" | grep -v '^pending=true$' || true; } > "$S/deposit_queue"
}
waiting() { # $1=age secs
  queue 4 256 cursor_version=2 deposit_next=6 deposit_final=4 backlog=4 oldest_waiting_index=6 oldest_waiting_enqueue_unix=1 \
    "oldest_waiting_age_secs=$1" oldest_waiting_deadline_secs=43200
}

# ---- the deposit-capable key (before a queue exists) --------------------------------------------------------------------------
out="$(run)"
expect "$out" '^SKIP: deposit-capable key — no vkey.json at .*guest/vkey.json \(./rollup guest-build writes it\)' "no vkey.json: skipped with the reason"
vkey_json none v0.2.0; out="$(run)"
expect "$out" '^SKIP: deposit-capable key — .*has no programVK' "a vkey.json without programVK is skipped with the reason"
vkey_json "$PVK" v0.2.0; registry "$(entry 0 active "$PVK" 900)"; out="$(run)"
expect "$out" "^PASS: deposit-capable key — the active key is $PVK, from guest v0.2.0" "the active key equals the programVK of a v0.2.0 guest: pass"
vkey_json "$PVK" v0.3.1; out="$(run)"
expect "$out" '^PASS: deposit-capable key — .*from guest v0.3.1' "a later guest tag passes"
vkey_json "$PVK" none; out="$(run)"; code=$?
expect "$out" '^WARN: deposit-capable key — the active key is '"$PVK"', but .*records no guest tag' "a vkey.json that records no guest tag warns (the key matches, the guest version cannot be told)"
[[ $code -eq 0 ]] && pass "the missing guest tag is a warning, not a failure" || fail "the missing guest tag is a warning, not a failure" "exit $code"
! grep -q '^PASS: deposit-capable key' <<<"$out" && pass "a vkey.json that records no guest tag does not pass" || fail "no guest tag does not pass" "$out"
vkey_json "$PVK" none; registry "$(entry 0 active "$OTHER" 900)"; out="$(run)"
expect "$out" '^FAIL: deposit-capable key — DepositKeyNotRegistered' "no guest tag and a key that is not registered still fails"
registry "$(entry 0 active "$PVK" 900)"
vkey_json "$PVK" v0.1.9; out="$(run)"; code=$?
expect "$out" '^FAIL: deposit-capable key — GuestNotDepositCapable: .*rome-zk-guest v0.1.9.*v0.2.0 or later' "a guest older than v0.2.0 fails by name"
[[ $code -ne 0 ]] && pass "an old guest makes check exit non-zero" || fail "an old guest makes check exit non-zero" "exit $code"
vkey_json "$PVK" v0.2.0; registry "$(entry 0 active "$OTHER" 900)"; out="$(run)"
expect "$out" "^FAIL: deposit-capable key — DepositKeyNotRegistered: no active registry key equals the programVK $PVK" "an active key that is not the guest's fails by name"
registry "$(entry 0 active "$OTHER" 900)" "$(entry 1 pending "$PVK" 5000)"; out="$(run)"
expect "$out" '^FAIL: deposit-capable key — DepositKeyNotActive: .*still pending' "the right key registered but pending fails by name"
registry "$(entry 0 retired "$PVK" retired)"; out="$(run)"
expect "$out" '^FAIL: deposit-capable key — DepositKeyNotRegistered' "a retired key does not count"
registry; out="$(run)"
expect "$out" '^FAIL: deposit-capable key — DepositKeyNotRegistered' "an empty registry fails by name"
touch "$S/vkey_show_fail"; out="$(run)"
expect "$out" '^FAIL: deposit-capable key — VkeyShowUnreadable: RegistryLookupFailed' "an unreadable registry is a named failure, never a pass"
rm "$S/vkey_show_fail"
registry "$(entry 0 active "$PVK" 900)"; queue 4 256; out="$(run)"
expect "$out" '^SKIP: deposit-capable key — the deposit queue exists already' "with a queue the key item is skipped with the reason"
rm -f "$S/deposit_queue"

# ---- the caps ----------------------------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^SKIP: deposit caps — no deposit queue yet' "no queue: the caps item is skipped with the reason"
queue 4 256; out="$(run)"
expect "$out" '^PASS: deposit caps — max_per_block 4 x blocks_per_batch 30 = 120, within max_per_batch 256' "caps that fit pass"
queue 8 256; out="$(run)"
expect "$out" '^PASS: deposit caps — .*= 240, within' "8 x 30 = 240 fits 256"
queue 9 256; out="$(run)"; code=$?
expect "$out" '^FAIL: deposit caps — DepositCapsExceedBatch: live max_per_block 9 x blocks_per_batch 30 = 270 is more than max_per_batch 256; run ./rollup deposit-queue propose --max-per-block 8' "caps that do not fit fail by name, with the number that does"
[[ $code -ne 0 ]] && pass "bad caps make check exit non-zero" || fail "bad caps make check exit non-zero" "exit $code"
queue 1 20; out="$(run)"
expect "$out" '^FAIL: deposit caps — DepositCapsExceedBatch: .*blocks_per_batch alone is more than max_per_batch' "a batch longer than the cap says blocks_per_batch is the problem"
queue 4 256 pending=true pending_max_per_batch=100 pending_max_per_block=4; out="$(run)"
expect "$out" '^FAIL: deposit caps — DepositCapsExceedBatch: pending max_per_block 4 x blocks_per_batch 30 = 120 is more than max_per_batch 100' "a pending proposal that does not fit fails by name too"
queue 4 256 pending=true pending_max_per_batch=256 pending_max_per_block=8; out="$(run)"
expect "$out" '^PASS: deposit caps' "a pending proposal that fits passes"
queue 4 256; cp "$ROLLUP_OUT/compose.env" "$WORK/compose.env.bpb"
sed -i.bak 's/^BLOCKS_PER_BATCH=.*/BLOCKS_PER_BATCH=/' "$ROLLUP_OUT/compose.env"; out="$(run)"; code=$?
expect "$out" "^FAIL: deposit caps — BlocksPerBatchUnreadable: .*no valid BLOCKS_PER_BATCH \\(got ''\\)" "an empty BLOCKS_PER_BATCH is refused by name"
[[ $code -ne 0 ]] && pass "an empty BLOCKS_PER_BATCH makes check exit non-zero" || fail "an empty BLOCKS_PER_BATCH makes check exit non-zero" "exit $code"
sed -i.bak 's/^BLOCKS_PER_BATCH=.*/BLOCKS_PER_BATCH=thirty/' "$ROLLUP_OUT/compose.env"; out="$(run)"
expect "$out" "^FAIL: deposit caps — BlocksPerBatchUnreadable: .*\\(got 'thirty'\\)" "a non-numeric BLOCKS_PER_BATCH is refused by name"
expect "$out" "^FAIL: inbox batches — BlocksPerBatchUnreadable: .*\\(got 'thirty'\\)" "a non-numeric BLOCKS_PER_BATCH also fails inbox batches by name, with or without a queue"
cp "$WORK/compose.env.bpb" "$ROLLUP_OUT/compose.env"; rm -f "$ROLLUP_OUT/compose.env.bak"
touch "$S/deposit_queue_fail"; out="$(run)"
expect "$out" '^FAIL: deposit caps — DepositQueueUnreadable: DepositQueueLookupFailed' "an unreadable queue is a named failure"
rm "$S/deposit_queue_fail" "$S/deposit_queue"

# ---- the genesis balance and the vault -----------------------------------------------------------------------------------------
genesis() { # $@=balances in wei (decimal), one per account
  python3 - "$ROLLUP_OUT/genesis.json" "$@" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
g["alloc"] = {"0x4200000000000000000000000000000000000016": {"balance": "0x0"}}
for i, w in enumerate(sys.argv[2:]):
    g["alloc"]["0x%040x" % (i + 1)] = {"balance": hex(int(w))}
json.dump(g, open(sys.argv[1], "w"))
PY
}
cp "$ROLLUP_OUT/genesis.json" "$WORK/genesis.keep"
out="$(run)"
expect "$out" '^PASS: genesis balance — the genesis has no balances' "a genesis with no balances passes"
genesis 5000000000000000000 1; out="$(run)"; code=$?
expect "$out" '^FAIL: genesis balance — GenesisFundedAccountLimit: .*gives 2 accounts a balance' "two balances fail by name"
[[ $code -ne 0 ]] && pass "two genesis balances make check exit non-zero" || fail "two genesis balances make check exit non-zero" "exit $code"
genesis 1500000001; out="$(run)"
expect "$out" '^FAIL: genesis balance — GenesisBalanceNotWholeLamports' "a balance that is not whole lamports fails by name"
genesis 5000000000000000000
at_genesis() { printf 'chain_id=4295391538\nslot=1000\nhead_pending_batch=0\nhead_final_batch=0\nreclaim_slots_left=500000\nvkey_entries=1\nvkey_active=yes\n' > "$S/chain_status"; }
bridge_cfg() { printf '  bridge_program (current)   %s\n  pending_mask                0 (none)\n  activation_slot             n/a (nothing pending)\n' "$1" > "$S/exit_config"; }
# A chain that has not set up deposits: a one-balance genesis, no vault, no exit config, no deposit queue. check passes.
rm -f "$S/deposit_queue" "$S/vault_show" "$S/chain_status"
printf '%s\n' "exit_config StubEc (chain 4295391538): no exit config (exits disabled)" > "$S/exit_config"; out="$(run)"; code=$?
expect "$out" '^WARN: genesis balance — VaultMissing: .*gives one account 5000000000 lamports but the chain has no vault.*only a warning until the deposit queue exists' "no queue, no vault: the genesis balance item warns, with the fix"
expect "$out" '^SKIP: exit config bridge — the chain has no exit config and no deposit queue yet' "no queue, no exit config: the bridge item is skipped with the reason"
! grep -q '^FAIL' <<<"$out" && pass "no queue, no exit config, no vault: no item fails" || fail "no queue, no exit config, no vault: no item fails" "$(grep '^FAIL' <<<"$out")"
[[ $code -eq 0 ]] && pass "no queue, no exit config, no vault: check passes (exit 0)" || fail "no queue, no exit config, no vault: check passes" "exit $code: $(grep '^FAIL' <<<"$out")"
# Once the queue exists a missing vault fails.
queue 4 256; out="$(run)"; code=$?
expect "$out" '^FAIL: genesis balance — VaultMissing: .*gives one account 5000000000 lamports but the chain has no vault' "a deposit queue and no vault fails by name, with the fix"
[[ $code -ne 0 ]] && pass "a deposit queue with no vault makes check exit non-zero" || fail "a deposit queue with no vault makes check exit non-zero" "exit $code"
# While the root is at genesis the vault is judged against the genesis balance; a short vault is a warning before the queue and a failure after it.
at_genesis
echo "  vault_token          StubToken  balance 4999999999 (raw units, 9 decimals)" > "$S/vault_show"; out="$(run)"
expect "$out" '^FAIL: genesis balance — VaultBelowGenesis: .*holds 4999999999 .*--amount 1 --confirm' "at genesis with a queue, a vault short by one lamport fails by name, with the amount to add"
rm "$S/deposit_queue"; out="$(run)"
expect "$out" '^WARN: genesis balance — VaultBelowGenesis: .*holds 4999999999' "at genesis with no queue yet, a short vault is a warning"
echo "  vault_token          StubToken  balance 5000000000 (raw units, 9 decimals)" > "$S/vault_show"; out="$(run)"
expect "$out" '^PASS: genesis balance — one balance of 5000000000 lamports, and the vault holds 5000000000' "at genesis, a vault that holds the balance passes"
# Posted but not yet final: an exit needs a final batch, so none can have run, and the vault is still judged. A queue can
# exist from the first posted batch, so this is the case where a short vault must fail.
printf 'chain_id=4295391538\nslot=1000\nhead_pending_batch=1\nhead_final_batch=0\nreclaim_slots_left=none\nvkey_entries=1\nvkey_active=yes\n' > "$S/chain_status"; queue 4 256
echo "  vault_token          StubToken  balance 4999999999 (raw units, 9 decimals)" > "$S/vault_show"; out="$(run)"; code=$?
expect "$out" '^FAIL: genesis balance — VaultBelowGenesis: .*holds 4999999999' "posted but not final, with a queue: a short vault fails by name (no exit can have run before a final batch)"
[[ $code -ne 0 ]] && pass "a short vault before the first final batch makes check exit non-zero" || fail "a short vault before the first final batch makes check exit non-zero" "exit $code"
rm "$S/deposit_queue"
# Past the first final batch exits can have moved the vault: the balance is reported, never judged.
rm "$S/chain_status"; queue 4 256; bridge_cfg "$BRIDGE"
echo "  vault_token          StubToken  balance 1200000000 (raw units, 9 decimals)" > "$S/vault_show"; out="$(run)"; code=$?
expect "$out" '^PASS: genesis balance — the genesis gave one account 5000000000 lamports; the vault holds 1200000000 now' "after exits the vault below the genesis balance is reported, not failed"
[[ $code -eq 0 ]] && pass "a vault below genesis after exits does not fail check" || fail "a vault below genesis after exits does not fail check" "exit $code: $(grep '^FAIL' <<<"$out")"
touch "$S/chain_status_fail"; out="$(run)"
expect "$out" '^PASS: genesis balance — the genesis gave one account 5000000000 lamports; the vault holds 1200000000 now' "with the root unreadable the balance is reported too (the chain status item fails)"
rm "$S/chain_status_fail"
echo "  vault_token          StubToken  not found (InitVault should have created it)" > "$S/vault_show"; out="$(run)"
expect "$out" '^FAIL: genesis balance — VaultUnreadable: no token balance' "a vault with no token account is a named failure"
touch "$S/vault_show_fail"; out="$(run)"
expect "$out" '^FAIL: genesis balance — VaultUnreadable: VaultConfigFetchFailed' "an unreadable vault is a named failure"
touch "$S/deposit_queue_fail"; rm "$S/vault_show_fail"; out="$(run)"
expect "$out" '^FAIL: genesis balance — DepositQueueUnreadable: DepositQueueLookupFailed' "an unreadable queue is a named failure here too: the item cannot tell whether a queue exists"
rm -f "$S/deposit_queue_fail" "$S/vault_show_fail" "$S/vault_show" "$S/deposit_queue" "$S/chain_status"; cp "$WORK/genesis.keep" "$ROLLUP_OUT/genesis.json"

# ---- the exit config names the bridge --------------------------------------------------------------------------------------------
bridge_cfg "$BRIDGE"; out="$(run)"
expect "$out" "^PASS: exit config bridge — the exit config names the bridge $BRIDGE" "an exit config that names the programs file's bridge passes"
bridge_cfg FixtureOtherBridge1111111111111111111111111112; out="$(run)"; code=$?
expect "$out" "^FAIL: exit config bridge — ExitConfigBridgeMismatch: the exit config names FixtureOtherBridge.*names $BRIDGE" "another bridge fails by name, with no queue yet"
[[ $code -ne 0 ]] && pass "a wrong bridge makes check exit non-zero" || fail "a wrong bridge makes check exit non-zero" "exit $code"
queue 4 256; out="$(run)"
expect "$out" '^FAIL: exit config bridge — ExitConfigBridgeMismatch' "another bridge fails by name with a queue too"
rm "$S/deposit_queue"
bridge_cfg 11111111111111111111111111111111; out="$(run)"
expect "$out" '^SKIP: exit config bridge — the exit config names no bridge and there is no deposit queue yet' "no bridge named and no queue: skipped with the reason"
queue 4 256; out="$(run)"
expect "$out" '^FAIL: exit config bridge — ExitConfigNamesNoBridge' "no bridge named with a queue fails by name"
rm "$S/deposit_queue"
printf '%s\n' "exit_config StubEc (chain 4295391538): no exit config (exits disabled)" > "$S/exit_config"; out="$(run)"
expect "$out" '^SKIP: exit config bridge — the chain has no exit config and no deposit queue yet' "no exit config and no queue: skipped with the reason"
queue 4 256; out="$(run)"
expect "$out" '^FAIL: exit config bridge — ExitConfigMissing' "no exit config with a queue fails by name"
rm "$S/deposit_queue" "$S/exit_config"; out="$(run)"
expect "$out" '^SKIP: exit config bridge — the exit config names no bridge and there is no deposit queue yet' "the stub's default exit config (no bridge) before a queue is skipped"
touch "$S/exit_config_fail"; out="$(run)"
expect "$out" '^SKIP: exit config bridge — needs the exit config, which could not be read' "an unreadable exit config is skipped here (the exit config item above fails)"
rm "$S/exit_config_fail"; bridge_cfg "$BRIDGE"
jq 'del(.programs["zk-bridge"])' "$FIX/programs.json" > "$WORK/nobridge.json"
out="$(PROGRAMS_JSON="$WORK/nobridge.json" run)"
expect "$out" '^SKIP: exit config bridge — needs programs.zk-bridge' "a programs file without the bridge: skipped with the reason"
expect "$out" '^SKIP: deposit caps — needs programs.zk-bridge' "and so is every item that reads the queue"

# ---- the [deposits] section --------------------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^SKIP: deposits section — no deposit queue yet' "no queue: the section item is skipped with the reason"
queue 4 256; out="$(run)"
expect "$out" '^PASS: deposits section — the deposit queue exists and .*has \[deposits\]' "a queue with the section rendered passes"
grep -v '^\[deposits\]$' "$ROLLUP_OUT/sequencer-config.toml" > "$WORK/seq.toml" && cp "$ROLLUP_OUT/sequencer-config.toml" "$WORK/seq.keep" && cp "$WORK/seq.toml" "$ROLLUP_OUT/sequencer-config.toml"
out="$(run)"; code=$?
expect "$out" '^FAIL: deposits section — DepositsSectionMissing: .*never credits a deposit \(run ./rollup init' "a queue without the section fails by name, with the fix"
[[ $code -ne 0 ]] && pass "a missing [deposits] section makes check exit non-zero" || fail "a missing [deposits] section makes check exit non-zero" "exit $code"
cp "$WORK/seq.keep" "$ROLLUP_OUT/sequencer-config.toml"; rm -f "$S/deposit_queue"

# ---- the cursor --------------------------------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^PASS: deposit cursor — the batch cursor is in the deposit-aware format \(version 2\)' "a version 2 cursor passes"
echo 1 > "$S/cursor_version"; out="$(run)"; code=$?
expect "$out" '^FAIL: deposit cursor — CursorNotV2: the batcher has finalized batches but the batch cursor is still version 1' "batches finalized and the cursor still version 1 fails by name"
[[ $code -ne 0 ]] && pass "a cursor stuck at version 1 makes check exit non-zero" || fail "a cursor stuck at version 1 makes check exit non-zero" "exit $code"
echo 0 > "$S/finalized_total"; out="$(run)"
expect "$out" '^SKIP: deposit cursor — the batch cursor is version 1; .*has not finalized one yet' "version 1 with no batch finalized since the batcher started: skipped with the reason"
echo "sequencer reth-verifier derive" > "$S/running"; echo 40 > "$S/finalized_total"; out="$(run)"
expect "$out" '^SKIP: deposit cursor — the batch cursor is version 1' "version 1 with no batcher running: skipped"
echo "sequencer batcher reth-verifier derive" > "$S/running"; echo 2 > "$S/cursor_version"
echo missing > "$S/cursor_next"; out="$(run)"
expect "$out" '^FAIL: deposit cursor — CursorUnreadable' "a cursor that cannot be read is a named failure"
echo 7 > "$S/cursor_next"
mv "$ROLLUP_OUT/pdas.env" "$ROLLUP_OUT/pdas.env.off"; out="$(run)"
expect "$out" '^SKIP: deposit cursor — needs pdas.env' "without pdas.env the cursor item is skipped with the reason"
mv "$ROLLUP_OUT/pdas.env.off" "$ROLLUP_OUT/pdas.env"

# ---- the oldest waiting deposit ------------------------------------------------------------------------------------------------------
out="$(run)"
expect "$out" '^SKIP: oldest deposit — no deposit queue yet' "no queue: skipped with the reason"
queue 4 256 cursor_version=2 deposit_next=10 deposit_final=10 backlog=0 oldest_waiting=none; out="$(run)"
expect "$out" '^PASS: oldest deposit — no deposit is waiting' "nothing waiting passes"
waiting 3600; out="$(run)"
expect "$out" '^PASS: oldest deposit — deposit 6 has waited 3600s of its 43200s deadline' "a young deposit passes"
waiting 21600; out="$(run)"
expect "$out" '^PASS: oldest deposit — deposit 6 has waited 21600s' "exactly half the deadline is not past half"
waiting 21601; out="$(run)"; code=$?
expect "$out" '^WARN: oldest deposit — deposit 6 has waited 21601s of its 43200s deadline \(more than half\)' "past half the deadline warns"
[[ $code -eq 0 ]] && pass "a warning does not fail the check" || fail "a warning does not fail the check" "exit $code: $out"
waiting 32400; out="$(run)"
expect "$out" '^WARN: oldest deposit' "exactly three quarters is still only a warning"
waiting 32401; out="$(run)"; code=$?
expect "$out" '^FAIL: oldest deposit — DepositNearDeadline: deposit 6 has waited 32401s of its 43200s deadline \(more than three quarters\)' "past three quarters fails by name"
[[ $code -ne 0 ]] && pass "a deposit near its deadline makes check exit non-zero" || fail "a deposit near its deadline makes check exit non-zero" "exit $code"
# A pending proposal that shortens the deadline counts while its activation slot is set; the stricter deadline is used.
pend_waiting() { # $1=age $2=pending deadline $3=pending=true|false
  queue 4 256 "pending=$3" pending_inclusion_deadline_secs="$2" cursor_version=2 deposit_next=6 deposit_final=4 backlog=4 oldest_waiting_index=6 \
    "oldest_waiting_age_secs=$1" oldest_waiting_deadline_secs=43200
}
pend_waiting 5401 7200 true; out="$(run)"
expect "$out" '^FAIL: oldest deposit — DepositNearDeadline: deposit 6 has waited 5401s of its 7200s deadline' "a pending deadline shorter than the active one is the one judged"
pend_waiting 5401 7200 false; out="$(run)"
expect "$out" '^PASS: oldest deposit — deposit 6 has waited 5401s of its 43200s deadline' "a pending deadline with no activation slot set is ignored"
pend_waiting 5401 86400 true; out="$(run)"
expect "$out" '^PASS: oldest deposit — deposit 6 has waited 5401s of its 43200s deadline' "a pending deadline longer than the active one leaves the active one"
pend_waiting 5401 none true; out="$(run)"
expect "$out" "^FAIL: oldest deposit — DepositAgeUnreadable: .*unreadable pending deadline 'none'" "a pending deadline that is not a number is a named failure"
queue 4 256 cursor_version=2 deposit_next=6 backlog=4 oldest_waiting=unreadable; out="$(run)"
expect "$out" '^FAIL: oldest deposit — DepositRecordUnreadable' "a record that cannot be read is a named failure"
queue 4 256 cursor_version=1; out="$(run)"
expect "$out" '^SKIP: oldest deposit — the batch cursor is not in the deposit-aware format yet' "a version 1 cursor: skipped with the reason"

# ---- the backlog ----------------------------------------------------------------------------------------------------------------------
waiting 10; out="$(run)"
expect "$out" '^PASS: deposit backlog — 4 waiting \(queue count 10, next to take 6\)' "the backlog is count minus deposit_next"
queue 4 256 cursor_version=1; out="$(run)"
expect "$out" '^SKIP: deposit backlog — the batch cursor is not in the deposit-aware format yet' "a version 1 cursor: skipped with the reason"
rm -f "$S/deposit_queue"; out="$(run)"
expect "$out" '^SKIP: deposit backlog — no deposit queue yet' "no queue: skipped with the reason"

# ---- no image ---------------------------------------------------------------------------------------------------------------------------
cp "$ROLLUP_OUT/compose.env" "$ROLLUP_OUT/compose.env.keep"
grep -v '^ROME_ZK_TAG=\|^ROME_ZK_IMAGE=' "$ROLLUP_OUT/compose.env.keep" > "$ROLLUP_OUT/compose.env"
for k in ROME_ZK_TAG ROME_ZK_IMAGE; do [[ -z "${!k:-}" ]] || unset "$k"; done
grep -v '^ROME_ZK_TAG=\|^ROME_ZK_IMAGE=' "$ROLLUP_ENV" > "$ROLLUP_ENV.new" && mv "$ROLLUP_ENV.new" "$ROLLUP_ENV"
out="$(run)"
expect "$out" '^FAIL: deposit caps — DepositQueueUnreadable: ImageTagNotSet' "no image: the deposit items name ImageTagNotSet"
mv "$ROLLUP_OUT/compose.env.keep" "$ROLLUP_OUT/compose.env"
finish check_deposit_items
