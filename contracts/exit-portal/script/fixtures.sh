#!/usr/bin/env bash
# contracts/exit-portal/script/fixtures.sh — generates fixtures/exit/* + RomeExitPortal.runtime.hex.
#
# Producer: run ONLY inside ghcr.io/foundry-rs/foundry:v1.3.6 (digest
# sha256:7026b071fb16606a14426acc0a0e2cd57f80b96abd7e7d371fef8dafe1712cdf, forge/anvil 1.3.6 commit
# d2415887) via `make exit-fixtures` on a build machine — never with a developer's own forge/anvil. The image
# carries forge/anvil/cast/sh/bash/grep/sed/awk only: no curl, no jq, no python3 — every RPC call below
# goes through `cast rpc` (which prints the unwrapped JSON-RPC `result` value directly) and every field
# extraction is grep/sed over that compact, single-line JSON.
#
# Determinism: a fixed, well-known test-only mnemonic (the standard anvil/hardhat dev mnemonic — no
# real funds ever touch it, and it never leaves this script) plus gas price 0 (the only balance change
# across the run is msg.value) means every byte this script writes reproduces exactly on a second run:
# `make exit-fixtures && git diff --exit-code fixtures/exit contracts/exit-portal/RomeExitPortal.runtime.hex`
# is CI's own last step for the `contracts` job.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORTAL_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$PORTAL_DIR/../.." && pwd)"
OUT_DIR="$REPO_ROOT/fixtures/exit"
mkdir -p "$OUT_DIR"

# Well-known, public, test-only (never a real key): the default anvil/hardhat dev mnemonic. Fixed here
# so every run derives the identical deployer address and every downstream fixture byte reproduces.
MNEMONIC="test test test test test test test test test test test junk"
RPC="http://127.0.0.1:8545"

# extract_field <json> <key> — first `"key":"value"` occurrence, value unquoted.
extract_field() {
  echo "$1" | grep -oE "\"$2\":\"[^\"]*\"" | head -1 | sed -E "s/\"$2\":\"([^\"]*)\"/\1/"
}
# extract_array <json> <key> — the `"key":[...]` span up to its first `]` (safe: every array this
# script reads holds only flat quoted-hex-string or flat-object elements, never a nested array).
extract_array() {
  echo "$1" | grep -oE "\"$2\":\[[^]]*\]"
}
# count_hex_nodes <array-json> — number of 0x... tokens in an extract_array result.
count_hex_nodes() {
  echo "$1" | grep -oE '0x[0-9a-fA-F]+' | wc -l | tr -d ' '
}
# max_hex_node_bytes <array-json...> — max (hexlen-2)/2 across every 0x... token in the given blob(s).
max_hex_node_bytes() {
  local max=0 tok len bytes
  for tok in $(echo "$1" | grep -oE '0x[0-9a-fA-F]+'); do
    len=${#tok}
    bytes=$(( (len - 2) / 2 ))
    (( bytes > max )) && max=$bytes
  done
  echo "$max"
}

cd "$PORTAL_DIR"

# --- runtime bytecode: a pure function of source + the pinned solc/optimizer settings in
# foundry.toml, independent of anvil state, so it can be built before anvil is even up. ---
forge build --deny-warnings >/dev/null
forge inspect RomeExitPortal deployedBytecode | tr -d '\n' > "$PORTAL_DIR/RomeExitPortal.runtime.hex"

# --- start anvil (mine-on-demand — the default; no --block-time). --timestamp pins the genesis
# timestamp: without it anvil seeds genesis from wall-clock time, which flows into every block header
# and makes block_hash (though not state_root — a pure function of account state) differ run to run,
# breaking the byte-for-byte determinism this fixture set promises. ---
anvil --chain-id 200101 --mnemonic "$MNEMONIC" --gas-price 0 --base-fee 0 --timestamp 1700000000 --silent &
ANVIL_PID=$!
cleanup() { kill "$ANVIL_PID" 2>/dev/null || true; }
trap cleanup EXIT

for _ in $(seq 1 50); do
  cast chain-id --rpc-url "$RPC" >/dev/null 2>&1 && break
  sleep 0.2
done

DEPLOYER_KEY="$(cast wallet private-key --mnemonic "$MNEMONIC" --mnemonic-index 0)"
DEPLOYER_ADDR="$(cast wallet address --private-key "$DEPLOYER_KEY")"

# --- deploy (deployer's first tx, nonce 0 -> deterministic address under the fixed mnemonic) ---
DEPLOY_OUT="$(forge create --rpc-url "$RPC" --private-key "$DEPLOYER_KEY" --broadcast src/RomeExitPortal.sol:RomeExitPortal)"
PORTAL="$(echo "$DEPLOY_OUT" | grep -E '^Deployed to:' | grep -oE '0x[0-9a-fA-F]{40}')"
[[ -n "$PORTAL" ]] || { echo "could not parse 'Deployed to:' address from: $DEPLOY_OUT" >&2; exit 1; }

# --- one initiateExit(solRecipient = 0x01 x32) with 1 ether from the deployer (portal's own first
# exit -> nonce 0) ---
SOL_RECIPIENT="0x$(printf '01%.0s' $(seq 1 32))"
AMOUNT_WEI="1000000000000000000"
ASSET="0x0000000000000000000000000000000000000000"
NONCE=0

cast send --rpc-url "$RPC" --private-key "$DEPLOYER_KEY" --value "$AMOUNT_WEI" \
  "$PORTAL" "initiateExit(bytes32)" "$SOL_RECIPIENT" >/dev/null

PREIMAGE="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$NONCE" "$DEPLOYER_ADDR" "$SOL_RECIPIENT" "$ASSET" "$AMOUNT_WEI")"
MESSAGE_HASH="$(cast keccak "$PREIMAGE")"
STORAGE_SLOT="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$MESSAGE_HASH" 0)")"

# --- exclusion counter-fixture: a message that was never sent (same shape, different nonce) ---
UNSENT_NONCE=999
UNSENT_PREIMAGE="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$UNSENT_NONCE" "$DEPLOYER_ADDR" "$SOL_RECIPIENT" "$ASSET" "$AMOUNT_WEI")"
UNSENT_HASH="$(cast keccak "$UNSENT_PREIMAGE")"
UNSENT_SLOT="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$UNSENT_HASH" 0)")"

# --- cross-check before writing anything: eth_getStorageAt agrees with the derived slot ---
STORED="$(cast storage "$PORTAL" "$STORAGE_SLOT" --rpc-url "$RPC")"
EXPECTED="0x0000000000000000000000000000000000000000000000000000000000000001"
[[ "$STORED" == "$EXPECTED" ]] || {
  echo "cross-check failed: eth_getStorageAt($PORTAL, $STORAGE_SLOT) = $STORED, expected $EXPECTED" >&2
  exit 1
}

# --- eth_getProof fixtures (cast rpc's own unwrapped `result` value, verbatim) ---
cast rpc eth_getProof "$PORTAL" "[\"$STORAGE_SLOT\"]" latest --rpc-url "$RPC" > "$OUT_DIR/anvil_getProof.json"
cast rpc eth_getProof "$PORTAL" "[\"$UNSENT_SLOT\"]" latest --rpc-url "$RPC" > "$OUT_DIR/anvil_getProof_unsent.json"

SENT_JSON="$(cat "$OUT_DIR/anvil_getProof.json")"
UNSENT_JSON="$(cat "$OUT_DIR/anvil_getProof_unsent.json")"
SENT_STORAGE_PROOF="$(extract_array "$SENT_JSON" storageProof)"
UNSENT_STORAGE_PROOF="$(extract_array "$UNSENT_JSON" storageProof)"
SENT_VALUE="$(extract_field "$SENT_STORAGE_PROOF" value)"
UNSENT_VALUE="$(extract_field "$UNSENT_STORAGE_PROOF" value)"

[[ "$SENT_VALUE" == "0x1" ]] || {
  echo "cross-check failed: anvil_getProof.json storageProof[0].value = $SENT_VALUE, expected 0x1" >&2
  exit 1
}
[[ "$UNSENT_VALUE" == "0x0" ]] || {
  echo "cross-check failed: anvil_getProof_unsent.json storageProof[0].value = $UNSENT_VALUE, expected 0x0 (exclusion)" >&2
  exit 1
}

# --- block / state root ---
BLOCK_JSON="$(cast rpc eth_getBlockByNumber latest false --rpc-url "$RPC")"
BLOCK_NUMBER="$(extract_field "$BLOCK_JSON" number)"
BLOCK_HASH="$(extract_field "$BLOCK_JSON" hash)"
STATE_ROOT="$(extract_field "$BLOCK_JSON" stateRoot)"

cat > "$OUT_DIR/anvil_state_root.json" <<JSON
{
  "number": "$BLOCK_NUMBER",
  "block_hash": "$BLOCK_HASH",
  "state_root": "$STATE_ROOT"
}
JSON

# --- rome-zk-exit-prover: eth_getLogs of the portal's own ExitInitiated event, plus the raw
# eth_getBlockByNumber response the prover's own decoder reads (BLOCK_JSON above, verbatim — the
# exit-prover parses the SAME shape `anvil_state_root.json`'s hand-picked fields were extracted from, but
# unextracted, since its own `eth_getBlockByNumber` client reads the whole object). One event on this
# portal (the base scenario's single `initiateExit`), so no topic filter is needed to isolate it.
cast rpc eth_getLogs "{\"address\":\"$PORTAL\",\"fromBlock\":\"0x0\",\"toBlock\":\"latest\"}" --rpc-url "$RPC" \
  > "$OUT_DIR/anvil_exit_logs.json"
echo "$BLOCK_JSON" > "$OUT_DIR/anvil_block_by_number.json"

# cross-check: exactly one log, its first topic (after the 0x-prefixed event selector topic0) equal to
# message_hash.json's own message_hash (the event's one indexed field) — printed nodes/bytes stats mirror
# the eth_getProof cross-checks above, never asserted silently.
LOGS_JSON="$(cat "$OUT_DIR/anvil_exit_logs.json")"
LOG_COUNT="$(echo "$LOGS_JSON" | grep -oE '"topics":\[[^]]*\]' | wc -l | tr -d ' ')"
[[ "$LOG_COUNT" == "1" ]] || {
  echo "cross-check failed: anvil_exit_logs.json carries $LOG_COUNT ExitInitiated logs, expected 1" >&2
  exit 1
}
LOG_MESSAGE_HASH_TOPIC="$(echo "$LOGS_JSON" | grep -oE '"topics":\["0x[0-9a-fA-F]*","0x[0-9a-fA-F]*"' | sed -E 's/.*,"(0x[0-9a-fA-F]*)"$/\1/')"
[[ "$LOG_MESSAGE_HASH_TOPIC" == "$MESSAGE_HASH" ]] || {
  echo "cross-check failed: anvil_exit_logs.json topics[1] = $LOG_MESSAGE_HASH_TOPIC, expected $MESSAGE_HASH (message_hash.json)" >&2
  exit 1
}
echo "exit logs: 1 ExitInitiated event, messageHash topic matches message_hash.json"

cat > "$OUT_DIR/message_hash.json" <<JSON
{
  "nonce": $NONCE,
  "l2_sender": "$DEPLOYER_ADDR",
  "sol_recipient": "$SOL_RECIPIENT",
  "asset": "$ASSET",
  "amount_wei": "$AMOUNT_WEI",
  "preimage_hex": "$PREIMAGE",
  "message_hash": "$MESSAGE_HASH",
  "storage_slot": "$STORAGE_SLOT",
  "portal": "$PORTAL"
}
JSON

# --- node counts + max node length (bounds: <= 64 nodes, <= 532 B/node) ---
SENT_ACCOUNT_PROOF="$(extract_array "$SENT_JSON" accountProof)"
ACC_NODES="$(count_hex_nodes "$SENT_ACCOUNT_PROOF")"
STO_NODES="$(count_hex_nodes "$(extract_array "$SENT_STORAGE_PROOF" proof)")"
MAX_NODE_BYTES="$(max_hex_node_bytes "$SENT_ACCOUNT_PROOF $(extract_array "$SENT_STORAGE_PROOF" proof)")"

echo "portal=$PORTAL message_hash=$MESSAGE_HASH storage_slot=$STORAGE_SLOT"
echo "accountProof nodes=$ACC_NODES storageProof nodes=$STO_NODES max_node_bytes=$MAX_NODE_BYTES"
echo "wrote $OUT_DIR/{message_hash.json,anvil_getProof.json,anvil_getProof_unsent.json,anvil_state_root.json} + $PORTAL_DIR/RomeExitPortal.runtime.hex"

# --- a second scenario, on a SECOND anvil instance (a fresh chain on its own port, left running alongside the
# first) so the files above are captured and finalized before this section ever runs — they stay byte-identical no
# matter what this section does. This portal sees 20 initiateExit calls (nonces 0..19, amounts (nonce+1) ether,
# recipients alternating 0x01x32/0x02x32) so its storage trie has real depth: multiple populated slots (branch
# nodes), a shared prefix worth skipping over in one hop (extension nodes), and — measured below, not assumed —
# whichever nodes are small enough to embed inline.
RPC2="http://127.0.0.1:8546"
anvil --chain-id 200101 --mnemonic "$MNEMONIC" --gas-price 0 --base-fee 0 --timestamp 1700000000 --port 8546 --silent &
ANVIL2_PID=$!
cleanup2() { kill "$ANVIL2_PID" 2>/dev/null || true; }
trap 'cleanup; cleanup2' EXIT

for _ in $(seq 1 50); do
  cast chain-id --rpc-url "$RPC2" >/dev/null 2>&1 && break
  sleep 0.2
done

DEPLOY_OUT2="$(forge create --rpc-url "$RPC2" --private-key "$DEPLOYER_KEY" --broadcast src/RomeExitPortal.sol:RomeExitPortal)"
PORTAL2="$(echo "$DEPLOY_OUT2" | grep -E '^Deployed to:' | grep -oE '0x[0-9a-fA-F]{40}')"
[[ -n "$PORTAL2" ]] || { echo "could not parse 'Deployed to:' address (multi) from: $DEPLOY_OUT2" >&2; exit 1; }

RECIPIENT_1="0x$(printf '01%.0s' $(seq 1 32))"
RECIPIENT_2="0x$(printf '02%.0s' $(seq 1 32))"
MULTI_SLOTS=()
MULTI_JSON_ROWS=""
for i in $(seq 0 19); do
  AMOUNT_I="$((i + 1))000000000000000000"
  if [[ $((i % 2)) -eq 0 ]]; then RECIP_I="$RECIPIENT_1"; else RECIP_I="$RECIPIENT_2"; fi

  cast send --rpc-url "$RPC2" --private-key "$DEPLOYER_KEY" --value "$AMOUNT_I" \
    "$PORTAL2" "initiateExit(bytes32)" "$RECIP_I" >/dev/null

  PREIMAGE_I="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$i" "$DEPLOYER_ADDR" "$RECIP_I" "$ASSET" "$AMOUNT_I")"
  HASH_I="$(cast keccak "$PREIMAGE_I")"
  SLOT_I="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$HASH_I" 0)")"
  MULTI_SLOTS+=("$SLOT_I")

  ROW="{\"nonce\":$i,\"l2_sender\":\"$DEPLOYER_ADDR\",\"sol_recipient\":\"$RECIP_I\",\"asset\":\"$ASSET\",\"amount_wei\":\"$AMOUNT_I\",\"message_hash\":\"$HASH_I\",\"storage_slot\":\"$SLOT_I\"}"
  if [[ -z "$MULTI_JSON_ROWS" ]]; then MULTI_JSON_ROWS="$ROW"; else MULTI_JSON_ROWS="$MULTI_JSON_ROWS,$ROW"; fi
done

# --- exit 7's slot (an inclusion proof) + a never-sent nonce on this same portal (an exclusion proof),
# in one eth_getProof call — this is the proof the `multi_fixture_has_branch_and_extension_nodes` test reads. ---
EXIT7_SLOT="${MULTI_SLOTS[7]}"
NEVER_SENT_NONCE=999
NEVER_SENT_PREIMAGE="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$NEVER_SENT_NONCE" "$DEPLOYER_ADDR" "$RECIPIENT_1" "$ASSET" "1000000000000000000")"
NEVER_SENT_HASH="$(cast keccak "$NEVER_SENT_PREIMAGE")"
NEVER_SENT_SLOT="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$NEVER_SENT_HASH" 0)")"

STORED7="$(cast storage "$PORTAL2" "$EXIT7_SLOT" --rpc-url "$RPC2")"
[[ "$STORED7" == "0x0000000000000000000000000000000000000000000000000000000000000001" ]] || {
  echo "cross-check failed (multi): eth_getStorageAt($PORTAL2, exit7 slot) = $STORED7, expected ...01" >&2
  exit 1
}
STORED_NEVER="$(cast storage "$PORTAL2" "$NEVER_SENT_SLOT" --rpc-url "$RPC2")"
[[ "$STORED_NEVER" == "0x0000000000000000000000000000000000000000000000000000000000000000" ]] || {
  echo "cross-check failed (multi): eth_getStorageAt($PORTAL2, never-sent slot) = $STORED_NEVER, expected 0" >&2
  exit 1
}

cast rpc eth_getProof "$PORTAL2" "[\"$EXIT7_SLOT\", \"$NEVER_SENT_SLOT\"]" latest --rpc-url "$RPC2" \
  > "$OUT_DIR/anvil_getProof_multi.json"

MULTI_JSON="$(cat "$OUT_DIR/anvil_getProof_multi.json")"
# Two requested keys -> two flat `{"key":...,"value":...,"proof":[...]}` objects in `storageProof`, in
# request order (exit 7's slot first, the never-sent slot second) — matched one object per output line so
# `sed -n '1p'`/`'2p'` picks each out individually, and `extract_field`/`extract_array` (already anchored
# on a specific key name) work unchanged on either one.
MULTI_STORAGE_ENTRIES="$(echo "$MULTI_JSON" | grep -oE '"key":"[^"]*","value":"[^"]*","proof":\[[^]]*\]')"
EXIT7_ENTRY="$(echo "$MULTI_STORAGE_ENTRIES" | sed -n '1p')"
NEVER_SENT_ENTRY="$(echo "$MULTI_STORAGE_ENTRIES" | sed -n '2p')"
EXIT7_VALUE="$(extract_field "$EXIT7_ENTRY" value)"
NEVER_SENT_VALUE="$(extract_field "$NEVER_SENT_ENTRY" value)"
[[ "$EXIT7_VALUE" == "0x1" ]] || {
  echo "cross-check failed: anvil_getProof_multi.json storageProof[0].value = $EXIT7_VALUE, expected 0x1" >&2
  exit 1
}
[[ "$NEVER_SENT_VALUE" == "0x0" ]] || {
  echo "cross-check failed: anvil_getProof_multi.json storageProof[1].value = $NEVER_SENT_VALUE, expected 0x0" >&2
  exit 1
}

BLOCK2_JSON="$(cast rpc eth_getBlockByNumber latest false --rpc-url "$RPC2")"
BLOCK2_NUMBER="$(extract_field "$BLOCK2_JSON" number)"
BLOCK2_HASH="$(extract_field "$BLOCK2_JSON" hash)"
STATE2_ROOT="$(extract_field "$BLOCK2_JSON" stateRoot)"

cat > "$OUT_DIR/anvil_state_root_multi.json" <<JSON
{
  "number": "$BLOCK2_NUMBER",
  "block_hash": "$BLOCK2_HASH",
  "state_root": "$STATE2_ROOT"
}
JSON

cat > "$OUT_DIR/message_hash_multi.json" <<JSON
{
  "portal": "$PORTAL2",
  "exit7_storage_slot": "$EXIT7_SLOT",
  "never_sent_nonce": $NEVER_SENT_NONCE,
  "never_sent_storage_slot": "$NEVER_SENT_SLOT",
  "messages": [$MULTI_JSON_ROWS]
}
JSON

MULTI_ACCOUNT_PROOF="$(extract_array "$MULTI_JSON" accountProof)"
MULTI_STO_PROOF_ARRAYS="$(extract_array "$MULTI_STORAGE_ENTRIES" proof)"
MULTI_ACC_NODES="$(count_hex_nodes "$MULTI_ACCOUNT_PROOF")"
MULTI_STO_NODES="$(count_hex_nodes "$MULTI_STO_PROOF_ARRAYS")"
MULTI_MAX_NODE_BYTES="$(max_hex_node_bytes "$MULTI_ACCOUNT_PROOF $MULTI_STO_PROOF_ARRAYS")"

echo "multi: portal=$PORTAL2 accountProof nodes=$MULTI_ACC_NODES storageProof nodes(both keys combined)=$MULTI_STO_NODES max_node_bytes=$MULTI_MAX_NODE_BYTES"
echo "wrote $OUT_DIR/{anvil_getProof_multi.json,anvil_state_root_multi.json,message_hash_multi.json}"

# --- a THIRD scenario, on a THIRD anvil instance (port 8547, again only after every earlier scenario's own files
# are already written), whose storage trie is deep enough to contain real Extension nodes (the classic forge vector
# for the walk's path-consume). 500 initiateExit calls (nonces 0..499, same portal contract, same deployer,
# recipients alternating 0x01x32/0x02x32 by parity, a flat 1-ether amount for every call -- kept simple since only
# the KEY distribution, not the amount, drives the trie's shape) reliably produces several Extension nodes (11
# distinct ones classified across a 750-key sweep during the offline grind below) -- unlike the 20-key `multi`
# scenario above, which was measured as having none.
#
# EXT_SENT_NONCE and EXT_UNSENT_NONCE below were found by an OFF-LINE grind (not reproduced at script
# run time, same pattern as `proof_for_other_address_is_refused`'s address grind): with this exact
# 500-call scenario running on the pinned image, every one of the 500 sent slots' storage proofs plus
# 250 never-sent candidate slots' proofs (nonces 500..749, same construction) were fetched in one batched
# `eth_getProof` call and each node classified by item count (17 = branch; 2 = leaf when the hex-prefix
# terminator flag is set, else extension). Nonce 21's inclusion proof is the first sent slot whose proof
# passes THROUGH a real Extension node (node kinds Branch, Branch, Extension, Branch, Leaf) into a
# Present leaf; nonce 516 (never sent) is the first exclusion proof whose walk DIVERGES AT a real
# Extension node (Branch, Branch, Extension -- the walk stops there, no further node needed). Re-run
# twice back to back on fresh anvil instances during the grind: portal address, account proof, and both
# storage proofs (bytes, not just shape) were identical -- this scenario is as deterministic as every
# other one in this file, the grind just avoids re-deriving 750 keys and reclassifying ~700 nodes on
# every `make exit-fixtures` invocation.
EXT_SENT_NONCE=21
EXT_UNSENT_NONCE=516
EXT_N_EXITS=500

RPC3="http://127.0.0.1:8547"
anvil --chain-id 200101 --mnemonic "$MNEMONIC" --gas-price 0 --base-fee 0 --timestamp 1700000000 --port 8547 --silent &
ANVIL3_PID=$!
cleanup3() { kill "$ANVIL3_PID" 2>/dev/null || true; }
trap 'cleanup; cleanup2; cleanup3' EXIT

for _ in $(seq 1 50); do
  cast chain-id --rpc-url "$RPC3" >/dev/null 2>&1 && break
  sleep 0.2
done

DEPLOY_OUT3="$(forge create --rpc-url "$RPC3" --private-key "$DEPLOYER_KEY" --broadcast src/RomeExitPortal.sol:RomeExitPortal)"
PORTAL3="$(echo "$DEPLOY_OUT3" | grep -E '^Deployed to:' | grep -oE '0x[0-9a-fA-F]{40}')"
[[ -n "$PORTAL3" ]] || { echo "could not parse 'Deployed to:' address (ext) from: $DEPLOY_OUT3" >&2; exit 1; }

for i in $(seq 0 $((EXT_N_EXITS - 1))); do
  if [[ $((i % 2)) -eq 0 ]]; then RECIP_I="$RECIPIENT_1"; else RECIP_I="$RECIPIENT_2"; fi
  cast send --rpc-url "$RPC3" --private-key "$DEPLOYER_KEY" --value "$AMOUNT_WEI" \
    "$PORTAL3" "initiateExit(bytes32)" "$RECIP_I" >/dev/null
done

if [[ $((EXT_SENT_NONCE % 2)) -eq 0 ]]; then EXT_SENT_RECIP="$RECIPIENT_1"; else EXT_SENT_RECIP="$RECIPIENT_2"; fi
if [[ $((EXT_UNSENT_NONCE % 2)) -eq 0 ]]; then EXT_UNSENT_RECIP="$RECIPIENT_1"; else EXT_UNSENT_RECIP="$RECIPIENT_2"; fi

EXT_SENT_PREIMAGE="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$EXT_SENT_NONCE" "$DEPLOYER_ADDR" "$EXT_SENT_RECIP" "$ASSET" "$AMOUNT_WEI")"
EXT_SENT_HASH="$(cast keccak "$EXT_SENT_PREIMAGE")"
EXT_SENT_SLOT="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$EXT_SENT_HASH" 0)")"

EXT_UNSENT_PREIMAGE="$(cast abi-encode "f(uint256,address,bytes32,address,uint256)" "$EXT_UNSENT_NONCE" "$DEPLOYER_ADDR" "$EXT_UNSENT_RECIP" "$ASSET" "$AMOUNT_WEI")"
EXT_UNSENT_HASH="$(cast keccak "$EXT_UNSENT_PREIMAGE")"
EXT_UNSENT_SLOT="$(cast keccak "$(cast abi-encode "f(bytes32,uint256)" "$EXT_UNSENT_HASH" 0)")"

STORED_EXT_SENT="$(cast storage "$PORTAL3" "$EXT_SENT_SLOT" --rpc-url "$RPC3")"
[[ "$STORED_EXT_SENT" == "0x0000000000000000000000000000000000000000000000000000000000000001" ]] || {
  echo "cross-check failed (ext): eth_getStorageAt($PORTAL3, nonce $EXT_SENT_NONCE slot) = $STORED_EXT_SENT, expected ...01" >&2
  exit 1
}
STORED_EXT_UNSENT="$(cast storage "$PORTAL3" "$EXT_UNSENT_SLOT" --rpc-url "$RPC3")"
[[ "$STORED_EXT_UNSENT" == "0x0000000000000000000000000000000000000000000000000000000000000000" ]] || {
  echo "cross-check failed (ext): eth_getStorageAt($PORTAL3, nonce $EXT_UNSENT_NONCE slot) = $STORED_EXT_UNSENT, expected 0" >&2
  exit 1
}

cast rpc eth_getProof "$PORTAL3" "[\"$EXT_SENT_SLOT\", \"$EXT_UNSENT_SLOT\"]" latest --rpc-url "$RPC3" \
  > "$OUT_DIR/anvil_getProof_ext.json"

EXT_JSON="$(cat "$OUT_DIR/anvil_getProof_ext.json")"
EXT_STORAGE_ENTRIES="$(echo "$EXT_JSON" | grep -oE '"key":"[^"]*","value":"[^"]*","proof":\[[^]]*\]')"
EXT_SENT_ENTRY="$(echo "$EXT_STORAGE_ENTRIES" | sed -n '1p')"
EXT_UNSENT_ENTRY="$(echo "$EXT_STORAGE_ENTRIES" | sed -n '2p')"
EXT_SENT_VALUE="$(extract_field "$EXT_SENT_ENTRY" value)"
EXT_UNSENT_VALUE="$(extract_field "$EXT_UNSENT_ENTRY" value)"
[[ "$EXT_SENT_VALUE" == "0x1" ]] || {
  echo "cross-check failed: anvil_getProof_ext.json storageProof[0].value = $EXT_SENT_VALUE, expected 0x1" >&2
  exit 1
}
[[ "$EXT_UNSENT_VALUE" == "0x0" ]] || {
  echo "cross-check failed: anvil_getProof_ext.json storageProof[1].value = $EXT_UNSENT_VALUE, expected 0x0" >&2
  exit 1
}

BLOCK3_JSON="$(cast rpc eth_getBlockByNumber latest false --rpc-url "$RPC3")"
BLOCK3_NUMBER="$(extract_field "$BLOCK3_JSON" number)"
BLOCK3_HASH="$(extract_field "$BLOCK3_JSON" hash)"
STATE3_ROOT="$(extract_field "$BLOCK3_JSON" stateRoot)"

cat > "$OUT_DIR/anvil_state_root_ext.json" <<JSON
{
  "number": "$BLOCK3_NUMBER",
  "block_hash": "$BLOCK3_HASH",
  "state_root": "$STATE3_ROOT"
}
JSON

cat > "$OUT_DIR/message_hash_ext.json" <<JSON
{
  "portal": "$PORTAL3",
  "n_exits": $EXT_N_EXITS,
  "ext_sent_nonce": $EXT_SENT_NONCE,
  "ext_sent_l2_sender": "$DEPLOYER_ADDR",
  "ext_sent_sol_recipient": "$EXT_SENT_RECIP",
  "ext_sent_asset": "$ASSET",
  "ext_sent_amount_wei": "$AMOUNT_WEI",
  "ext_sent_message_hash": "$EXT_SENT_HASH",
  "ext_sent_storage_slot": "$EXT_SENT_SLOT",
  "ext_unsent_nonce": $EXT_UNSENT_NONCE,
  "ext_unsent_sol_recipient": "$EXT_UNSENT_RECIP",
  "ext_unsent_message_hash": "$EXT_UNSENT_HASH",
  "ext_unsent_storage_slot": "$EXT_UNSENT_SLOT"
}
JSON

EXT_ACCOUNT_PROOF="$(extract_array "$EXT_JSON" accountProof)"
EXT_STO_PROOF_ARRAYS="$(extract_array "$EXT_STORAGE_ENTRIES" proof)"
EXT_ACC_NODES="$(count_hex_nodes "$EXT_ACCOUNT_PROOF")"
EXT_STO_NODES="$(count_hex_nodes "$EXT_STO_PROOF_ARRAYS")"
EXT_MAX_NODE_BYTES="$(max_hex_node_bytes "$EXT_ACCOUNT_PROOF $EXT_STO_PROOF_ARRAYS")"

echo "ext: portal=$PORTAL3 accountProof nodes=$EXT_ACC_NODES storageProof nodes(both keys combined)=$EXT_STO_NODES max_node_bytes=$EXT_MAX_NODE_BYTES"
echo "wrote $OUT_DIR/{anvil_getProof_ext.json,anvil_state_root_ext.json,message_hash_ext.json}"
