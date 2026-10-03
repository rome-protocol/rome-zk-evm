#!/usr/bin/env bash
# deploy/rollup/tests/outsider/outsider_path_local.sh — the outside path, end to end, on one machine.
#
# What a stranger does with this repository, run against a local Solana test validator instead of a public cluster:
#
#   1. build the node image from this checkout (or use NODE_IMAGE)
#   2. start solana-test-validator with zk-inbox, zk-settlement, veritas and zk-bridge loaded from the build-sbf output
#   3. set up the settlement program's global config the way the public one was set up: init-global-config, then
#      set-global-config with permissionless registration on, then show, and refuse unless every field is as asked
#   4. ./rollup init, ./rollup register --confirm (permissionless), ./rollup up (no prover)
#   5. send a few L2 transactions
#   6. wait until a batch is finalized on the inbox (the program logs "batch <chain>/<n> finalized")
#   7. check that derive rebuilt the same chain: the verifier node's head is the sequencer's head, the block hash is the
#      same on both, the recipient's balance on the verifier is what was sent, and net_peerCount is 0
#   8. ./rollup check passes, and the running verifier carries the four no-peer flags
#   9. tear everything down
#
# It needs: bash, docker with the compose plugin, curl, jq, openssl, python3 (3.11 or newer), rustup/cargo, and the Solana
# tools (solana-test-validator, solana, solana-keygen). The programs come from `cargo build-sbf --arch v3` (target/deploy
# by default). Nothing here reaches a public Solana cluster or a public Ethereum network: the validator listens on the
# docker bridge address only, the node's own compose file keeps its ports on 127.0.0.1, and the verifier runs with peer
# discovery off.
#
# Usage:   deploy/rollup/tests/outsider/outsider_path_local.sh
# Settings (environment):
#   PROGRAMS_DIR           where zk_inbox.so, zk_settlement.so, veritas.so, zk_bridge.so are  default <repo>/target/deploy
#   NODE_IMAGE             a node image already built from this checkout; otherwise one is built and removed at the end
#   FOUNDRY_IMAGE          the image that holds `cast`, used to sign and send L2 transactions
#   FINALIZE_WAIT_SECS     how long to wait for a finalized batch                      default 300
#   DERIVE_WAIT_SECS       how long to wait for the verifier to reach the sequencer    default 240
#   KEEP=1                 leave the stack, the validator and the work directory running (for debugging)
#   WORK_DIR               use this directory for keys, ledger and rendered files (created, and removed at the end,
#                          unless it already exists)
#
# Host ports: it picks a free one for every port setting of the node's compose file (RPC_PORT, WS_PORT, VERIFIER_RPC_PORT
# and the four *_METRICS_PORT settings in deploy/rollup/.env.example) and for the validator (RPC, websocket, faucet,
# gossip and a block of dynamic ports), all from 20000-32000, and its compose project is named outsider-path-<pid>. It
# refuses by name when that project already has containers. Two runs, or a run and any other stack, can therefore share a
# machine without a clash.
#
# Every refusal starts with a CamelCase name on stderr and the script exits non-zero. On a failure it prints the tail of
# the validator log and of every container log before it tears down.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROLLUP_DIR="$(cd "$HERE/../.." && pwd)"
ROOT="$(cd "$ROLLUP_DIR/../.." && pwd)"
PROGRAMS_DIR="${PROGRAMS_DIR:-$ROOT/target/deploy}"
FOUNDRY_IMAGE="${FOUNDRY_IMAGE:-ghcr.io/foundry-rs/foundry:v1.3.6}"
FINALIZE_WAIT_SECS="${FINALIZE_WAIT_SECS:-300}"
DERIVE_WAIT_SECS="${DERIVE_WAIT_SECS:-240}"
KEEP="${KEEP:-0}"

# Host ports are picked in the preflight, once the helpers below exist.
L2_TXS=3
L2_SEND_WEI=10000000000000000   # 0.01 ether per transaction

START_SECS=$SECONDS
VALIDATOR_PID=""
BUILT_IMAGE=""
WORK_CREATED=0
PROJECT="outsider-path-$$"
FAILED=1   # cleared only by a clean finish

say() { echo "[$(( SECONDS - START_SECS ))s] $*"; }
die() { echo "$1: $2" >&2; exit 1; }

# ---------------------------------------------------------------------------------------------------- teardown
dump_logs() {
  echo "---- failure: the last lines of every log ----" >&2
  if [[ -n "${VALIDATOR_LOG:-}" && -f "$VALIDATOR_LOG" ]]; then
    echo "-- validator ($VALIDATOR_LOG)" >&2; tail -n 30 "$VALIDATOR_LOG" >&2
  fi
  local c
  for c in $(docker ps -a --filter "label=com.docker.compose.project=$PROJECT" --format '{{.Names}}' 2>/dev/null); do
    echo "-- container $c" >&2; docker logs --tail 40 "$c" 2>&1 | cut -c1-300 >&2
  done
}

teardown() {
  local rc=$?
  trap - EXIT INT TERM
  [[ $FAILED -eq 0 ]] || { [[ $rc -ne 0 ]] || rc=1; }
  if [[ $FAILED -ne 0 ]]; then dump_logs; fi
  if [[ "$KEEP" == 1 ]]; then
    echo "KEEP=1: leaving project $PROJECT, the validator (pid ${VALIDATOR_PID:-none}) and ${WORK:-no work directory} in place" >&2
    exit "$rc"
  fi
  say "tearing down"
  # Only what this run created: its own compose project (by label), its own validator process, its own directory and
  # the image it built.
  if [[ -f "${OUT:-/nonexistent}/compose.env" ]]; then
    docker compose -p "$PROJECT" --env-file "$OUT/compose.env" -f "$ROLLUP_DIR/docker-compose.yml" down -v --remove-orphans >/dev/null 2>&1
  fi
  local ids
  ids="$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
  [[ -z "$ids" ]] || docker rm -f $ids >/dev/null 2>&1
  ids="$(docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
  [[ -z "$ids" ]] || docker volume rm -f $ids >/dev/null 2>&1
  ids="$(docker network ls -q --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)"
  [[ -z "$ids" ]] || docker network rm $ids >/dev/null 2>&1
  if [[ -n "$VALIDATOR_PID" ]] && kill -0 "$VALIDATOR_PID" 2>/dev/null; then
    kill "$VALIDATOR_PID" 2>/dev/null
    local i; for i in $(seq 1 20); do kill -0 "$VALIDATOR_PID" 2>/dev/null || break; sleep 0.5; done
    kill -9 "$VALIDATOR_PID" 2>/dev/null
  fi
  [[ -z "$BUILT_IMAGE" ]] || docker rmi -f "$BUILT_IMAGE" >/dev/null 2>&1
  [[ $WORK_CREATED -eq 0 || -z "${WORK:-}" ]] || rm -rf "$WORK"
  if [[ $FAILED -eq 0 ]]; then
    echo "OUTSIDER PATH: PASS in $(( SECONDS - START_SECS ))s"
  else
    echo "OUTSIDER PATH: FAIL after $(( SECONDS - START_SECS ))s" >&2
  fi
  exit "$rc"
}
trap teardown EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---------------------------------------------------------------------------------------------------- helpers
# jrpc URL METHOD PARAMS_JSON -> the whole JSON-RPC answer
jrpc() {
  curl -sS --max-time 15 -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$2\",\"params\":${3:-[]}}" "$1" 2>/dev/null
}
jresult() { jq -r '.result // empty' 2>/dev/null; }

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# pick_ports COUNT -> sets PICKED to the first port of COUNT consecutive ports, none of which is listening on this machine or is one
# this run already took. They come from 20000-32000, below the kernel's ephemeral range, so nothing hands them out
# meanwhile. A run that races this one for the same ports loses at `docker compose up` or at the validator's start, by name.
TAKEN_PORTS=" "
pick_ports() {
  local count="$1" tries=0 base i ok
  while (( tries++ < 500 )); do
    base=$(( 20000 + RANDOM % 11900 ))
    ok=1
    for (( i = 0; i < count; i++ )); do
      if [[ "$TAKEN_PORTS" == *" $((base + i)) "* ]] || port_busy $((base + i)); then ok=0; break; fi
    done
    if (( ok )); then
      for (( i = 0; i < count; i++ )); do TAKEN_PORTS+="$((base + i)) "; done
      PICKED="$base"; return 0
    fi
  done
  return 1
}

wait_for() { # wait_for SECONDS DESCRIPTION COMMAND... -> 0 when the command succeeds within the time
  local secs="$1" what="$2"; shift 2
  local end=$(( SECONDS + secs ))
  while (( SECONDS < end )); do
    "$@" && return 0
    sleep 2
  done
  echo "timed out after ${secs}s waiting for $what" >&2
  return 1
}

# ---------------------------------------------------------------------------------------------------- preflight
say "preflight"
for c in docker curl jq openssl python3 cargo solana solana-keygen solana-test-validator; do
  command -v "$c" >/dev/null 2>&1 || die ToolMissing "$c is not on PATH"
done
docker compose version >/dev/null 2>&1 || die ToolMissing "docker compose plugin is not available"
docker info >/dev/null 2>&1 || die DockerUnreachable "the docker daemon does not answer (is this user allowed to use it?)"
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' || die PythonTooOld "rollup needs python3 3.11 or newer (got $(python3 --version 2>&1))"
for f in zk_inbox zk_settlement veritas zk_bridge; do
  [[ -s "$PROGRAMS_DIR/$f.so" ]] || die ProgramMissing "$PROGRAMS_DIR/$f.so is absent (build the programs: cargo build-sbf --manifest-path programs/<name>/Cargo.toml --sbf-out-dir target/deploy --arch v3)"
done
# A compose project of this name with anything in it means a stale or a foreign stack: never act on it.
[[ -z "$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null)" ]] \
  || die ProjectInUse "compose project $PROJECT already has containers on this machine; refusing to touch them"
# Every host port this run publishes, free ones picked now. The names are the node's own settings (deploy/rollup/.env.example).
NODE_PORT_VARS=(RPC_PORT WS_PORT VERIFIER_RPC_PORT SEQUENCER_METRICS_PORT BATCHER_METRICS_PORT DERIVE_METRICS_PORT PROVER_METRICS_PORT)
for v in "${NODE_PORT_VARS[@]}"; do
  pick_ports 1 || die NoFreePort "no free port found for $v in 20000-32000"
  printf -v "$v" '%s' "$PICKED"
done
pick_ports 2 || die NoFreePort "no free port pair for the validator's RPC and websocket"   # the websocket is RPC + 1
SOL_RPC_PORT="$PICKED"
pick_ports 1 || die NoFreePort "no free port for the validator's faucet"; SOL_FAUCET_PORT="$PICKED"
pick_ports 1 || die NoFreePort "no free port for the validator's gossip"; SOL_GOSSIP_PORT="$PICKED"
pick_ports 40 || die NoFreePort "no free block of 40 ports for the validator"; SOL_DYNAMIC_BASE="$PICKED"
SOL_DYNAMIC_PORTS="$SOL_DYNAMIC_BASE-$((SOL_DYNAMIC_BASE + 39))"
SEQ_RPC="http://127.0.0.1:$RPC_PORT"
VER_RPC="http://127.0.0.1:$VERIFIER_RPC_PORT"
say "host ports: rpc $RPC_PORT, ws $WS_PORT, verifier $VERIFIER_RPC_PORT, metrics $SEQUENCER_METRICS_PORT/$BATCHER_METRICS_PORT/$DERIVE_METRICS_PORT/$PROVER_METRICS_PORT; validator rpc $SOL_RPC_PORT, faucet $SOL_FAUCET_PORT, gossip $SOL_GOSSIP_PORT, dynamic $SOL_DYNAMIC_PORTS"

# The validator listens on the docker bridge address: the host reaches it there, so do the node's containers, and no other
# machine can. (A container's own 127.0.0.1 is the container, so a loopback URL would not work for the batcher.)
BRIDGE_IP="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null)"
[[ "$BRIDGE_IP" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || die DockerBridgeUnknown "cannot read the docker bridge network's gateway address (docker network inspect bridge)"
SOL_RPC="http://$BRIDGE_IP:$SOL_RPC_PORT"

if [[ -n "${WORK_DIR:-}" ]]; then
  WORK="$WORK_DIR"
  [[ -e "$WORK" ]] || { mkdir -p "$WORK" || die WorkDirFailed "cannot create $WORK"; WORK_CREATED=1; }
else
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/outsider-path.XXXXXX")" || die WorkDirFailed "mktemp failed"
  WORK_CREATED=1
fi
chmod 700 "$WORK"
KEYS="$WORK/keys"; OUT="$WORK/rendered"; VALIDATOR_LOG="$WORK/validator.log"
mkdir -p "$KEYS" "$OUT"

# ---------------------------------------------------------------------------------------------------- 1. image
if [[ -n "${NODE_IMAGE:-}" ]]; then
  IMAGE="$NODE_IMAGE"
  docker image inspect "$IMAGE" >/dev/null 2>&1 || die ImageMissing "NODE_IMAGE $IMAGE is not in the local docker image store"
  say "using the node image $IMAGE"
else
  IMAGE="rome-zk-evm:outsider-path-$$"
  say "building the node image $IMAGE from this checkout"
  docker build -q -t "$IMAGE" "$ROOT" >/dev/null || die ImageBuildFailed "docker build of the node image failed"
  BUILT_IMAGE="$IMAGE"
  say "node image built"
fi

# ---------------------------------------------------------------------------------------------------- 2. keys
say "generating keys (throwaway: they only ever hold local-validator funds)"
for k in payer registry authority; do
  solana-keygen new --no-bip39-passphrase --silent --force --outfile "$KEYS/$k.json" >/dev/null || die KeygenFailed "solana-keygen failed for $k"
done
PAYER_PK="$(solana-keygen pubkey "$KEYS/payer.json")"
REGISTRY_PK="$(solana-keygen pubkey "$KEYS/registry.json")"
AUTHORITY_PK="$(solana-keygen pubkey "$KEYS/authority.json")"
# The node's containers run as uid 999 and read the sequencer key and the payer keypair through bind mounts, so those two
# files are world-readable. The directory is 0700, so nobody else on the machine can reach them.
( umask 022; openssl rand -hex 32 > "$KEYS/sequencer.key" )
chmod 644 "$KEYS/payer.json" "$KEYS/sequencer.key"
chmod 600 "$KEYS/registry.json" "$KEYS/authority.json"

# The L2 account that holds the genesis balance and sends the transactions. It is created here, by cast, and its key stays
# in this shell's memory.
l2json="$(docker run --rm --entrypoint cast "$FOUNDRY_IMAGE" wallet new --json 2>/dev/null)" || die CastFailed "could not create an L2 account with cast ($FOUNDRY_IMAGE)"
L2_ADDR="$(jq -r '.[0].address' <<<"$l2json")"
L2_KEY="$(jq -r '.[0].private_key' <<<"$l2json")"
unset l2json
[[ "$L2_ADDR" =~ ^0x[0-9a-fA-F]{40}$ && -n "$L2_KEY" && "$L2_KEY" != null ]] || die CastFailed "cast wallet new gave no usable account"
RECIPIENT="0x$(openssl rand -hex 20)"

# ---------------------------------------------------------------------------------------------------- 3. validator
read_program_id() { jq -r --arg n "$1" '.programs[$n] // empty' "$ROLLUP_DIR/programs.devnet.json"; }
INBOX_ID="$(read_program_id zk-inbox)"; SETTLEMENT_ID="$(read_program_id zk-settlement)"; VERITAS_ID="$(read_program_id veritas)"; BRIDGE_ID="$(read_program_id zk-bridge)"
[[ -n "$INBOX_ID" && -n "$SETTLEMENT_ID" && -n "$VERITAS_ID" && -n "$BRIDGE_ID" ]] || die ProgramsFileInvalid "$ROLLUP_DIR/programs.devnet.json lacks one of zk-inbox, zk-settlement, veritas, zk-bridge"
# The programs check each other's addresses, so they load at the published addresses, with a key of this run's as the upgrade
# authority (init-global-config needs the real one).
jq -n --arg i "$INBOX_ID" --arg s "$SETTLEMENT_ID" --arg v "$VERITAS_ID" --arg b "$BRIDGE_ID" \
  '{cluster: "localnet", programs: {"zk-inbox": $i, "zk-settlement": $s, "veritas": $v, "zk-bridge": $b}}' > "$WORK/programs.json"

say "starting solana-test-validator on $BRIDGE_IP:$SOL_RPC_PORT"
solana-test-validator --reset --quiet \
  --ledger "$WORK/ledger" \
  --bind-address "$BRIDGE_IP" --rpc-port "$SOL_RPC_PORT" --faucet-port "$SOL_FAUCET_PORT" \
  --gossip-port "$SOL_GOSSIP_PORT" --dynamic-port-range "$SOL_DYNAMIC_PORTS" \
  --upgradeable-program "$INBOX_ID" "$PROGRAMS_DIR/zk_inbox.so" "$AUTHORITY_PK" \
  --upgradeable-program "$SETTLEMENT_ID" "$PROGRAMS_DIR/zk_settlement.so" "$AUTHORITY_PK" \
  --upgradeable-program "$VERITAS_ID" "$PROGRAMS_DIR/veritas.so" "$AUTHORITY_PK" \
  --upgradeable-program "$BRIDGE_ID" "$PROGRAMS_DIR/zk_bridge.so" "$AUTHORITY_PK" \
  >"$VALIDATOR_LOG" 2>&1 &
VALIDATOR_PID=$!
validator_up() {
  kill -0 "$VALIDATOR_PID" 2>/dev/null || { echo "the validator exited" >&2; return 2; }
  [[ "$(jrpc "$SOL_RPC" getHealth | jresult)" == ok ]]
}
wait_for 120 "the validator to report healthy" validator_up || die ValidatorNotUp "solana-test-validator did not come up"
for pid in "$INBOX_ID" "$SETTLEMENT_ID" "$VERITAS_ID" "$BRIDGE_ID"; do
  [[ "$(jrpc "$SOL_RPC" getAccountInfo "[\"$pid\",{\"encoding\":\"base64\"}]" | jq -r '.result.value.executable // empty')" == true ]] \
    || die ProgramNotLoaded "$pid is not an executable account on the local validator"
done
say "validator up; the four programs (zk-inbox, zk-settlement, veritas, zk-bridge) are loaded and executable"
for k in payer registry authority; do
  pk="$(solana-keygen pubkey "$KEYS/$k.json")"
  solana airdrop 100 "$pk" --url "$SOL_RPC" >/dev/null 2>&1 || solana airdrop 100 "$pk" --url "$SOL_RPC" >/dev/null || die AirdropFailed "could not fund the $k key from the local faucet"
done

# ---------------------------------------------------------------------------------------------------- 4. global config
# The same three steps as the public one: init (permissionless registration starts off), set with it on, show and compare.
DEPOSIT_LAMPORTS=5000000000
FEE_BASE_LAMPORTS=1000000
FEE_BPS=0
RECLAIM_WINDOW_SLOTS=216000
gov() { (cd "$ROOT" && cargo run --quiet -p zk-settlement-client --features devnet-driver --example governance -- "$@" --rpc-url "$SOL_RPC"); }
say "setting up the settlement global config (this compiles the client the first time)"
gov init-global-config --settlement "$SETTLEMENT_ID" \
  --authority-keypair "$KEYS/authority.json" --payer-keypair "$KEYS/payer.json" \
  --registry-authority "$REGISTRY_PK" --treasury "$AUTHORITY_PK" \
  --reclaim-window-slots "$RECLAIM_WINDOW_SLOTS" --deposit-lamports "$DEPOSIT_LAMPORTS" \
  --fee-base-lamports "$FEE_BASE_LAMPORTS" --fee-bps "$FEE_BPS" || die GlobalConfigInitFailed "init-global-config failed"
gov set-global-config --settlement "$SETTLEMENT_ID" --registry-keypair "$KEYS/registry.json" \
  --permissionless-init-enabled true \
  --reclaim-window-slots "$RECLAIM_WINDOW_SLOTS" --deposit-lamports "$DEPOSIT_LAMPORTS" \
  --fee-base-lamports "$FEE_BASE_LAMPORTS" --fee-bps "$FEE_BPS" || die GlobalConfigSetFailed "set-global-config failed"
shown="$(gov show --settlement "$SETTLEMENT_ID")" || die GlobalConfigShowFailed "show failed"
gfield() { awk -v k="$1" '$1 == k { print $2; exit }' <<<"$shown"; }
gexpect() { [[ "$(gfield "$1")" == "$2" ]] || die GlobalConfigMismatch "$1 expected $2 got '$(gfield "$1")'"; }
gexpect registry_authority "$REGISTRY_PK"
gexpect treasury "$AUTHORITY_PK"
gexpect permissionless_init_enabled true
gexpect reclaim_window_slots "$RECLAIM_WINDOW_SLOTS"
gexpect deposit_lamports "$DEPOSIT_LAMPORTS"
gexpect default_fee_base_lamports "$FEE_BASE_LAMPORTS"
gexpect default_fee_bps "$FEE_BPS"
say "global config verified: every field is as asked"

# ---------------------------------------------------------------------------------------------------- 5. init, register
cat > "$WORK/chain.toml" <<EOF
[genesis]
fee_recipient = "0x1111111111111111111111111111111111111111"
backed_address = "$L2_ADDR"
backed_balance_lamports = 1_000_000_000

[profile]
sub_block_ms = 50
sub_blocks_per_block = 20
sub_block_gas_limit = 2_000_000
blocks_per_batch = 60
empty_block_interval_secs = 0

[batcher]
batch_close_after_secs = 10
EOF
cat > "$WORK/.env" <<EOF
SOLANA_RPC_URL=$SOL_RPC
PROGRAMS_JSON=$WORK/programs.json
SEQUENCER_KEY_PATH=$KEYS/sequencer.key
PAYER_KEYPAIR_PATH=$KEYS/payer.json
RPC_BIND=127.0.0.1
ROME_ZK_IMAGE=$IMAGE
RPC_PORT=$RPC_PORT
WS_PORT=$WS_PORT
VERIFIER_RPC_PORT=$VERIFIER_RPC_PORT
SEQUENCER_METRICS_PORT=$SEQUENCER_METRICS_PORT
BATCHER_METRICS_PORT=$BATCHER_METRICS_PORT
DERIVE_METRICS_PORT=$DERIVE_METRICS_PORT
PROVER_METRICS_PORT=$PROVER_METRICS_PORT
EOF
export ROLLUP_ENV="$WORK/.env" CHAIN_TOML="$WORK/chain.toml" ROLLUP_OUT="$OUT"
# One compose project per run, so this run's containers and volumes never mix with anything else's.
export COMPOSE_PROJECT_NAME="$PROJECT"
ROLLUP="$ROLLUP_DIR/rollup"

say "./rollup init"
# The chain id comes from the payer key's nonce account, and about half of all keys get one above what wallets such as
# MetaMask accept, which init refuses by name (ChainIdNotWalletSafe). A throwaway payer is cheap, so draw another key
# (funded from the local faucet) until the id is acceptable; any other failure stops the run.
for attempt in $(seq 1 30); do
  init_out="$("$ROLLUP" init 2>&1)" && { printf '%s\n' "$init_out"; break; }
  if grep -q '^ChainIdNotWalletSafe' <<<"$init_out" && (( attempt < 30 )); then
    say "payer key $attempt got a chain id wallets cannot add; drawing another"
    solana-keygen new --no-bip39-passphrase --silent --force --outfile "$KEYS/payer.json" >/dev/null || die KeygenFailed "solana-keygen failed for the payer"
    chmod 644 "$KEYS/payer.json"
    PAYER_PK="$(solana-keygen pubkey "$KEYS/payer.json")"
    solana airdrop 100 "$PAYER_PK" --url "$SOL_RPC" >/dev/null 2>&1 || solana airdrop 100 "$PAYER_PK" --url "$SOL_RPC" >/dev/null || die AirdropFailed "could not fund the new payer key from the local faucet"
    continue
  fi
  printf '%s\n' "$init_out" >&2
  die InitFailed "./rollup init failed"
done
CHAIN_ID="$(jq -r '.config.chainId' "$OUT/genesis.json")"
# Every compose call ./rollup makes takes its project name from COMPOSE_PROJECT_NAME. If something between this shell and
# docker drops that variable (a sudo wrapper, for one), compose falls back to the directory name and would act on any
# other stack of that name on the machine, so refuse before anything is started.
seen_project="$(docker compose --env-file "$OUT/compose.env" -f "$ROLLUP_DIR/docker-compose.yml" config --format json 2>/dev/null | jq -r '.name // empty')"
[[ "$seen_project" == "$PROJECT" ]] || die ComposeProjectLost "docker compose resolves the project name '$seen_project', not $PROJECT; refusing to start anything (it could act on another stack of that name)"
say "./rollup register --confirm (chain $CHAIN_ID, permissionless)"
"$ROLLUP" register --confirm || die RegisterFailed "./rollup register --confirm failed"
[[ -s "$OUT/pdas.env" ]] || die RegisterFailed "register did not write $OUT/pdas.env"

# ---------------------------------------------------------------------------------------------------- 6. up
say "./rollup up (no prover)"
"$ROLLUP" up || die UpFailed "./rollup up failed"
running() { docker compose -p "$PROJECT" --env-file "$OUT/compose.env" -f "$ROLLUP_DIR/docker-compose.yml" ps --services --status running 2>/dev/null; }
all_four_up() { local r s; r="$(running)"; for s in sequencer reth-verifier derive batcher; do grep -qx "$s" <<<"$r" || return 1; done; }
wait_for 180 "sequencer, reth-verifier, derive and batcher to run" all_four_up || die ServicesNotUp "the four node services are not all running"
sequencer_up() { [[ -n "$(jrpc "$SEQ_RPC" eth_chainId | jresult)" ]]; }
wait_for 120 "the sequencer's RPC" sequencer_up || die SequencerNotUp "the sequencer does not answer on $SEQ_RPC"

# ---------------------------------------------------------------------------------------------------- 7. L2 transactions
say "sending $L2_TXS L2 transactions from $L2_ADDR to $RECIPIENT"
export ETH_PRIVATE_KEY="$L2_KEY"
for i in $(seq 1 "$L2_TXS"); do
  # cast (v1.3.6) reads no key from the environment, so the container's own shell hands it the key it was given as
  # ETH_PRIVATE_KEY (docker passes the value from this shell without it appearing in this host's command line).
  out="$(docker run --rm --network host -e ETH_PRIVATE_KEY --entrypoint sh "$FOUNDRY_IMAGE" \
          -c 'exec cast send --private-key "$ETH_PRIVATE_KEY" "$@"' cast \
          --rpc-url "$SEQ_RPC" "$RECIPIENT" --value "$L2_SEND_WEI" --json 2>&1)"
  status="$(jq -r '.status // empty' <<<"$out" 2>/dev/null)"
  block="$(jq -r '.blockNumber // empty' <<<"$out" 2>/dev/null)"
  case "$status" in 1|0x1|success) ;; *) die L2SendFailed "transaction $i did not succeed: $(tail -c 300 <<<"$out")" ;; esac
  say "L2 transaction $i mined in block $((block))"
  sleep 1
done
unset ETH_PRIVATE_KEY L2_KEY

# The sequencer seals blocks only when there are transactions (empty_block_interval_secs = 0), so its head stops here.
SEQ_HEAD="$(jrpc "$SEQ_RPC" eth_blockNumber | jresult)"
sleep 2
[[ "$(jrpc "$SEQ_RPC" eth_blockNumber | jresult)" == "$SEQ_HEAD" ]] || die SequencerHeadMoving "the sequencer kept producing blocks after the last transaction"
say "sequencer head is block $((SEQ_HEAD))"

# ---------------------------------------------------------------------------------------------------- 8. FinalizeBatch
# The inbox logs "batch <chain>/<n> finalized, <leaves> leaves, ..." in its FinalizeBatch transaction. Look for it in the
# program's own transactions.
FINALIZED_SIG=""; FINALIZED_LOG=""
finalized_seen() {
  local sigs sig logs
  sigs="$(jrpc "$SOL_RPC" getSignaturesForAddress "[\"$INBOX_ID\",{\"limit\":200,\"commitment\":\"confirmed\"}]" | jq -r '.result[]? | select(.err == null) | .signature')"
  for sig in $sigs; do
    logs="$(jrpc "$SOL_RPC" getTransaction "[\"$sig\",{\"encoding\":\"json\",\"maxSupportedTransactionVersion\":255,\"commitment\":\"confirmed\"}]" \
            | jq -r '.result.meta.logMessages[]? | select(test("batch [0-9]+/[0-9]+ finalized"))' | head -1)"
    if [[ -n "$logs" ]]; then FINALIZED_SIG="$sig"; FINALIZED_LOG="$logs"; return 0; fi
  done
  return 1
}
say "waiting up to ${FINALIZE_WAIT_SECS}s for FinalizeBatch on the inbox"
wait_for "$FINALIZE_WAIT_SECS" "FinalizeBatch on the inbox" finalized_seen || die NoBatchFinalized "no batch was finalized on the inbox within ${FINALIZE_WAIT_SECS}s"
say "FinalizeBatch seen: $FINALIZED_SIG ($FINALIZED_LOG)"

# ---------------------------------------------------------------------------------------------------- 9. derive head
verifier_head_matches() { [[ "$(jrpc "$VER_RPC" eth_blockNumber | jresult)" == "$SEQ_HEAD" ]]; }
say "waiting up to ${DERIVE_WAIT_SECS}s for the verifier (fed by derive) to reach block $((SEQ_HEAD))"
wait_for "$DERIVE_WAIT_SECS" "the verifier's head to equal the sequencer's head" verifier_head_matches \
  || die DeriveHeadBehind "the verifier is at $(jrpc "$VER_RPC" eth_blockNumber | jresult), the sequencer at $SEQ_HEAD"
seq_hash="$(jrpc "$SEQ_RPC" eth_getBlockByNumber "[\"$SEQ_HEAD\",false]" | jq -r '.result.hash // empty')"
ver_hash="$(jrpc "$VER_RPC" eth_getBlockByNumber "[\"$SEQ_HEAD\",false]" | jq -r '.result.hash // empty')"
[[ -n "$seq_hash" && "$seq_hash" == "$ver_hash" ]] || die DeriveHashDiffers "block $((SEQ_HEAD)) has hash $seq_hash on the sequencer and '$ver_hash' on the verifier"
say "derive head equals the sequencer head: block $((SEQ_HEAD)), hash $seq_hash"
ver_balance="$(jrpc "$VER_RPC" eth_getBalance "[\"$RECIPIENT\",\"latest\"]" | jresult)"
want_balance=$(( L2_TXS * L2_SEND_WEI ))
[[ "$ver_balance" =~ ^0x[0-9a-fA-F]+$ ]] && (( ver_balance == want_balance )) || die DeriveStateDiffers "the recipient holds '$ver_balance' on the verifier, expected $want_balance wei"
say "the recipient's balance on the verifier is $want_balance wei, as sent"

# ---------------------------------------------------------------------------------------------------- 10. no peers
peers="$(jrpc "$VER_RPC" net_peerCount | jresult)"
[[ "$peers" == 0x0 ]] || die VerifierHasPeers "net_peerCount on the verifier is '$peers', expected 0x0"
say "reth-verifier net_peerCount is 0"
ver_cid="$(docker ps -q --filter "label=com.docker.compose.project=$PROJECT" --filter "label=com.docker.compose.service=reth-verifier")"
[[ -n "$ver_cid" ]] || die VerifierNotFound "no running reth-verifier container in project $PROJECT"
ver_args="$(docker inspect --format '{{join .Args " "}}' "$ver_cid")"
for flag in "--disable-discovery" "--max-outbound-peers 0" "--max-inbound-peers 0" "--addr 127.0.0.1"; do
  grep -qF -- "$flag" <<<"$ver_args" || die VerifierFlagMissing "the running reth-verifier does not carry $flag (args: $ver_args)"
done
ver_ports="$(docker port "$ver_cid" 2>/dev/null)"
if grep -qE '30303|30304' <<<"$ver_ports"; then die VerifierPublishesP2p "the running reth-verifier publishes a devp2p port: $ver_ports"; fi
say "reth-verifier runs with --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 --addr 127.0.0.1 and publishes no p2p port"

# ---------------------------------------------------------------------------------------------------- 11. rollup check
say "./rollup check"
"$ROLLUP" check || die CheckFailed "./rollup check reported a failure"

FAILED=0
exit 0
