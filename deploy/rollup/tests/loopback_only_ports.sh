#!/usr/bin/env bash
# deploy/rollup/tests/loopback_only_ports.sh — the verifier node runs reth's `testing` API, which can include
# arbitrary transactions, and derive drives it; neither may ever be reachable from outside the host. Every port they
# (and the batcher, prover and database) publish is bound to 127.0.0.1, and the engine API (8551) is not published at
# all. The sequencer is the public RPC: its bind address is the operator's choice (RPC_BIND, loopback by default).
# Two independent checks: the compose file's text, and `docker compose config` when docker is present.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
[[ -f "$COMPOSE" ]] || { fail "docker-compose.yml exists" "missing"; finish loopback_only_ports; }

# Text check: the `ports:` entries of one service block.
service_ports() { # $1=service -> one published-port string per line
  awk -v svc="$1" '
    /^  [a-z0-9-]+:/ { cur=$1; sub(":","",cur); inports=0; next }
    cur==svc && /^    ports:/ { inports=1; next }
    cur==svc && inports && /^      - / { sub(/^      - /,""); gsub(/["'"'"']/,""); sub(/ *#.*/,""); print; next }
    inports && !/^      - / && !/^ *#/ { inports=0 }
  ' "$COMPOSE"
}
for svc in reth-verifier derive batcher prover postgres; do
  ports="$(service_ports "$svc")"
  bad="$(grep -vE '^127\.0\.0\.1:' <<<"$ports" | grep -v '^$' || true)"
  [[ -z "$bad" ]] && pass "$svc publishes only on 127.0.0.1 (${ports:-no ports})" || fail "$svc publishes only on 127.0.0.1" "$bad"
done
[[ -z "$(service_ports postgres)" ]] && pass "postgres publishes no host port (only the prover reaches it, over the compose network)" || fail "postgres publishes no host port" "$(service_ports postgres)"
grep -qE '8551' <<<"$(service_ports reth-verifier)" && fail "the engine API (8551) is not published" "reth-verifier publishes 8551" || pass "the engine API (8551) is not published"
grep -qE '^\$\{RPC_BIND[:-]' <<<"$(service_ports sequencer | head -1)" && pass "sequencer RPC bind comes from RPC_BIND" || fail "sequencer RPC bind comes from RPC_BIND" "$(service_ports sequencer | head -1)"
grep -qE '^RPC_BIND=127\.0\.0\.1$' "$ROLLUP_DIR/.env.example" && pass ".env.example keeps RPC_BIND on loopback" || fail ".env.example keeps RPC_BIND on loopback" "$(grep RPC_BIND "$ROLLUP_DIR/.env.example")"

# Config check, when docker compose is available (CI has it).
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  cfg="$(cd "$ROLLUP_DIR" && SEQUENCER_KEY_PATH=/x PAYER_KEYPAIR_PATH=/x CHAIN_ID=1 BLOCK_GAS_LIMIT=1 SOLANA_RPC_URL=x PROVER_DB_PASSWORD_FILE=/x docker compose --profile prover -f docker-compose.yml config --format json 2>&1)"
  if jq -e . >/dev/null 2>&1 <<<"$cfg"; then
    pass "docker compose config renders (all profiles)"
    ports="$(jq -r '.services | to_entries[] | .key as $s | (.value.ports // [])[] | "\($s) \(.published) \(.host_ip // "")"' <<<"$cfg")"
    for svc in reth-verifier derive batcher prover postgres; do
      bad="$(awk -v s="$svc" '$1==s && $3!="127.0.0.1" {print}' <<<"$ports")"
      [[ -z "$bad" ]] && pass "compose config: $svc is loopback-only" || fail "compose config: $svc is loopback-only" "$bad"
    done
    jq -e '[.services[].image // empty] | map(test("prom/|grafana/|node-exporter")) | any | not' >/dev/null <<<"$cfg" && pass "no monitoring image in the stack" || fail "no monitoring image in the stack" "found one"
    jq -e '[.services | keys[]] | (index("sequencer") != null and index("batcher") != null and index("reth-verifier") != null and index("derive") != null)' >/dev/null <<<"$cfg" && pass "the four node services are present" || fail "the four node services are present" "$(jq -c '.services|keys' <<<"$cfg")"
    jq -e '.services.prover.profiles == ["prover"] and (.services.prover.gpus != null or .services.prover.deploy != null)' >/dev/null <<<"$cfg" && pass "the prover is opt-in (profile) and asks for a GPU" || fail "the prover is opt-in (profile) and asks for a GPU" "$(jq -c '.services.prover|{profiles,gpus}' <<<"$cfg")"
  else
    fail "docker compose config renders" "$(head -c 400 <<<"$cfg")"
  fi
else
  echo "SKIP: docker compose config checks — docker compose not available on this host (the text checks above ran)"
fi
finish loopback_only_ports
