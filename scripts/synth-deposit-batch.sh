#!/usr/bin/env bash
# scripts/synth-deposit-batch.sh — builds the two synthetic deposit batches (fixtures/prover-input/synthetic-deposits-
# {small,full}.{bin,json}) from a fresh local reth node per batch.
#
#   scripts/synth-deposit-batch.sh <reth-binary> <synth-deposit-batch-binary> <genesis.json> <out-dir> <fetched-at-unix>
#
# Build the generator with its feature: cd crates/rome-zk-prover-input && cargo build --release --locked --features synth
#   --bin synth-deposit-batch
#
# The node never joins a public peer network: peer discovery off, no outbound or inbound peers, p2p on loopback and no
# published port; the JSON-RPC and Engine API listen on loopback only. The Engine API's JWT secret is generated per run
# (mode 0600, removed with the node's data directory); nothing here is a signing key or a funded account.
set -euo pipefail

RETH="${1:?reth binary}"
SYNTH="${2:?synth-deposit-batch binary}"
GENESIS="${3:?genesis.json}"
OUT="${4:?output directory}"
FETCHED_AT="${5:?fetched-at (unix seconds)}"
HTTP_PORT="${SYNTH_HTTP_PORT:-18547}"
AUTH_PORT="${SYNTH_AUTH_PORT:-18551}"

RUN="$(mktemp -d)"
PID=""
cleanup() {
  [[ -n "$PID" ]] && kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null || true
  rm -rf "$RUN"
}
trap cleanup EXIT

umask 077
openssl rand -hex 32 >"$RUN/jwt.hex"
mkdir -p "$OUT"

for shape in small full; do
  datadir="$RUN/data-$shape"
  "$RETH" node \
    --chain "$GENESIS" \
    --datadir "$datadir" \
    --disable-discovery --max-outbound-peers 0 --max-inbound-peers 0 --addr 127.0.0.1 \
    --http --http.addr 127.0.0.1 --http.port "$HTTP_PORT" --http.api eth,net,web3,testing,debug \
    --authrpc.addr 127.0.0.1 --authrpc.port "$AUTH_PORT" --authrpc.jwtsecret "$RUN/jwt.hex" \
    --builder.gaslimit 40000000 --rpc.eth-proof-window 1000000 \
    >"$RUN/reth-$shape.log" 2>&1 &
  PID=$!
  for _ in $(seq 1 60); do
    if curl -sf -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"net_peerCount","params":[]}' "http://127.0.0.1:$HTTP_PORT" >/dev/null; then
      break
    fi
    sleep 1
  done
  "$SYNTH" --shape "$shape" --genesis "$GENESIS" \
    --rpc "http://127.0.0.1:$HTTP_PORT" --engine "http://127.0.0.1:$AUTH_PORT" --jwt-secret "$RUN/jwt.hex" \
    --out "$OUT/synthetic-deposits-$shape.bin" --fetched-at "$FETCHED_AT"
  kill "$PID"; wait "$PID" 2>/dev/null || true
  PID=""
  rm -rf "$datadir"
done
