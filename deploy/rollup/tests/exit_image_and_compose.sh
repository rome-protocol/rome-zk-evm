#!/usr/bin/env bash
# deploy/rollup/tests/exit_image_and_compose.sh — withdrawals need the exit prover in the node image and a compose
# service for it. The image carries the binary beside the others, the service starts only with EXITS=on, runs from the
# node image, mounts its config and a payer key of its own (never the batcher's), talks to the verifier node and Solana, serves
# metrics on loopback and never publishes a peer-to-peer port.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
DOCKERFILE="$REPO_ROOT/Dockerfile"
REAL_DOCKER="$(command -v docker || true)"   # the stub docker that setup_fixture puts on PATH would shadow it

# 1. The image builds and installs the binary.
grep -qE 'cargo build .*-p rome-zk-exit-prover' <(tr -d '\\\n' < "$DOCKERFILE" | tr -s ' ') && pass "the Dockerfile builds rome-zk-exit-prover with the other binaries" || fail "the Dockerfile builds rome-zk-exit-prover" "no -p rome-zk-exit-prover in the cargo build line"
grep -qE 'cp .*target/release-host/rome-zk-exit-prover ' <(tr -d '\\\n' < "$DOCKERFILE" | tr -s ' ') && pass "the build copies the binary out of the cache mount" || fail "the build copies the binary out of the cache mount" "not in the cp line"
grep -q '^COPY --from=builder /out/rome-zk-exit-prover /usr/local/bin/rome-zk-exit-prover$' "$DOCKERFILE" && pass "the runtime stage installs it in /usr/local/bin" || fail "the runtime stage installs it in /usr/local/bin" "no COPY line"

# 2. The compose service.
svc="$(awk '/^  exit-prover:/{p=1;print;next} /^  [a-z0-9-]+:/{p=0} /^volumes:/{p=0} p' "$COMPOSE")"
[[ -n "$svc" ]] && pass "compose has an exit-prover service" || { fail "compose has an exit-prover service" "missing"; finish exit_image_and_compose; }
grep -q '<<: \*node' <<<"$svc" && pass "exit-prover runs from the node image" || fail "exit-prover runs from the node image" "no <<: *node"
grep -qE 'profiles: \["exits"\]' <<<"$svc" && pass "exit-prover is in the exits profile (EXITS=on)" || fail "exit-prover is in the exits profile" "$svc"
grep -q 'entrypoint: \["rome-zk-exit-prover"\]' <<<"$svc" && pass "its entrypoint is rome-zk-exit-prover" || fail "its entrypoint is rome-zk-exit-prover" "missing"
grep -q '"--config", "/data/exit-prover.toml"' <<<"$svc" && pass "it reads /data/exit-prover.toml" || fail "it reads /data/exit-prover.toml" "missing"
grep -qE 'exit-prover\.toml:/data/exit-prover\.toml:ro' <<<"$svc" && pass "the rendered config is mounted read-only" || fail "the rendered config is mounted read-only" "missing"
grep -qE '\$\{EXIT_PAYER_KEYPAIR_PATH[^}]*\}:/run/rome-zk/payer\.json:ro' <<<"$svc" && pass "the exit payer's key is mounted read-only for the exit prover" || fail "the exit payer's key is mounted read-only" "missing"
grep -q 'PAYER_KEYPAIR_PATH' <(sed 's/EXIT_PAYER_KEYPAIR_PATH//g' <<<"$svc") && fail "the batcher's payer key is never mounted into the exit prover" "$(grep -n PAYER_KEYPAIR_PATH <<<"$svc")" || pass "the batcher's payer key is never mounted into the exit prover"
grep -qE '^# EXIT_PAYER_KEYPAIR_PATH=' "$ROLLUP_DIR/.env.example" && pass ".env.example lists EXIT_PAYER_KEYPAIR_PATH" || fail ".env.example lists EXIT_PAYER_KEYPAIR_PATH" "missing"
grep -qE '^# *EXIT_PAYER_FLOOR_LAMPORTS' "$ROLLUP_DIR/.env.example" && pass ".env.example documents EXIT_PAYER_FLOOR_LAMPORTS" || fail ".env.example documents EXIT_PAYER_FLOOR_LAMPORTS" "missing"
grep -qE 'healthcheck:' <<<"$svc" && grep -A 1 'healthcheck:' <<<"$svc" | grep -q 'disable: true' && pass "the image's sequencer probe is switched off for it" || fail "the image's sequencer probe is switched off" "missing"
grep -qE '"127\.0\.0\.1:\$\{EXIT_PROVER_METRICS_PORT:-9005\}:9005"' <<<"$svc" && pass "metrics are published on 127.0.0.1 only, from EXIT_PROVER_METRICS_PORT (default 9005)" || fail "metrics port" "$(grep -n 9005 <<<"$svc")"
grep -q 'reth-verifier:' <<<"$svc" && grep -q 'service_healthy' <<<"$svc" && pass "it waits for the verifier node" || fail "it waits for the verifier node" "no depends_on"
ports="$(grep -E '^\s+- "' <<<"$svc")"
! grep -qE '30303|0\.0\.0\.0:' <<<"$ports" && [[ "$(wc -l <<<"$ports" | tr -d ' ')" == 1 ]] && pass "it publishes nothing but its metrics, and no peer-to-peer port" || fail "it publishes nothing but its metrics" "$ports"
grep -qE 'restart|<<: \*node' <<<"$svc" && pass "restart policy and log rotation come from the shared node settings" || fail "restart and logging" "missing"
grep -qE '^EXIT_PROVER_METRICS_PORT=9005( |$)' "$ROLLUP_DIR/.env.example" && pass ".env.example lists EXIT_PROVER_METRICS_PORT=9005" || fail ".env.example lists EXIT_PROVER_METRICS_PORT=9005" "missing"
grep -qE '^# EXITS=on' "$ROLLUP_DIR/.env.example" && pass ".env.example documents EXITS=on" || fail ".env.example documents EXITS=on" "missing"

# 3. The resolved config, when docker compose is available: off by default, on with the profile, no peer-to-peer port.
if [[ -n "$REAL_DOCKER" ]] && "$REAL_DOCKER" compose version >/dev/null 2>&1; then
  env_=(SEQUENCER_KEY_PATH=/x PAYER_KEYPAIR_PATH=/payer EXIT_PAYER_KEYPAIR_PATH=/exit-payer CHAIN_ID=1 BLOCK_GAS_LIMIT=1 PROVER_DB_PASSWORD_FILE=/x)
  base="$(cd "$ROLLUP_DIR" && env "${env_[@]}" "$REAL_DOCKER" compose -f docker-compose.yml config --format json 2>&1)"
  jq -e '.services | has("exit-prover") | not' >/dev/null 2>&1 <<<"$base" && pass "compose config: no exit-prover without the profile" || fail "compose config: no exit-prover without the profile" "$(head -c 300 <<<"$base")"
  on="$(cd "$ROLLUP_DIR" && env "${env_[@]}" "$REAL_DOCKER" compose --profile exits -f docker-compose.yml config --format json 2>&1)"
  jq -e '.services["exit-prover"].ports | length == 1 and (.[0].host_ip == "127.0.0.1") and (.[0].target == 9005)' >/dev/null 2>&1 <<<"$on" && pass "compose config: one port, 9005 on 127.0.0.1" || fail "compose config: the exit-prover port" "$(jq -c '.services["exit-prover"].ports' <<<"$on" 2>&1 | head -c 300)"
  jq -e '[.services["exit-prover"].volumes[] | select(.target == "/run/rome-zk/payer.json") | .source] == ["/exit-payer"]' >/dev/null 2>&1 <<<"$on" && pass "compose config: the exit prover gets the exit payer's key, not the batcher's" || fail "compose config: the exit prover's key" "$(jq -c '.services["exit-prover"].volumes' <<<"$on" 2>&1 | head -c 300)"
  jq -e '[.services["exit-prover"].volumes[].source] | index("/payer") == null' >/dev/null 2>&1 <<<"$on" && pass "compose config: the batcher's payer path appears nowhere in the exit prover's mounts" || fail "compose config: the batcher's payer in the exit prover" "present"
  # Without EXITS the exit key is not set, and the file still resolves.
  env_nokey=(SEQUENCER_KEY_PATH=/x PAYER_KEYPAIR_PATH=/payer CHAIN_ID=1 BLOCK_GAS_LIMIT=1 PROVER_DB_PASSWORD_FILE=/x)
  nokey="$(cd "$ROLLUP_DIR" && env "${env_nokey[@]}" "$REAL_DOCKER" compose -f docker-compose.yml config --format json 2>&1)"
  jq -e '.services | has("exit-prover") | not' >/dev/null 2>&1 <<<"$nokey" && pass "compose config: no exit payer variable is needed while EXITS is off" || fail "compose config without EXIT_PAYER_KEYPAIR_PATH" "$(head -c 300 <<<"$nokey")"
  jq -e '.services["exit-prover"].depends_on["reth-verifier"].condition == "service_healthy"' >/dev/null 2>&1 <<<"$on" && pass "compose config: it waits for a healthy verifier" || fail "compose config: depends_on" "missing"
else
  echo "SKIP: compose config checks for the exit prover (docker compose not available; the text checks above ran)"
fi
finish exit_image_and_compose
