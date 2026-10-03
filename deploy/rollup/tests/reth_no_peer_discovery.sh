#!/usr/bin/env bash
# deploy/rollup/tests/reth_no_peer_discovery.sh — every stock reth node in the compose file runs with peer discovery
# off and no peer slots. The verifier is fed only by derive over the Engine API and never needs a peer; with discovery
# on it joins the public Ethereum peer network and dials hundreds of nodes, which cloud providers flag as crypto
# activity.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
[[ -f "$COMPOSE" ]] || { fail "compose file exists" "missing"; finish reth_no_peer_discovery; }

# Each service block: from "  <name>:" to the next two-space service header.
services="$(awk '/^  [a-z][a-z0-9-]*:$/ { svc=$1; sub(":","",svc) } svc != "" && /image: *ghcr\.io\/paradigmxyz\/reth/ { print svc }' "$COMPOSE" | sort -u)"
[[ -n "$services" ]] || { fail "the compose file has a stock reth service" "none found"; finish reth_no_peer_discovery; }
for svc in $services; do
  block="$(awk -v s="$svc" '/^  [a-z][a-z0-9-]*:$/ { cur=$1; sub(":","",cur) } cur == s' "$COMPOSE")"
  for flag in "--disable-discovery" "--max-outbound-peers 0" "--max-inbound-peers 0" "--addr 127.0.0.1"; do
    if grep -qF -- "$flag" <<<"$block"; then pass "$svc runs with $flag"; else fail "$svc runs with $flag" "missing from its command"; fi
  done
done
if grep -nE ':3030[34]' "$COMPOSE" >/dev/null; then fail "no devp2p port is published" "$(grep -nE ':3030[34]' "$COMPOSE" | head -2 | tr '\n' ' ')"; else pass "no devp2p port is published"; fi
finish reth_no_peer_discovery
