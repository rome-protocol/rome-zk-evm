#!/usr/bin/env bash
# deploy/rollup/tests/host_ports_from_env.sh — the host side of every published port comes from .env, with today's
# numbers as the defaults, so a chain started without any of these settings behaves as it always did and a second
# chain (or a test run) can sit beside it. Checked four ways: the compose file's text, .env.example, what `init`
# renders, and `docker compose config` and `./rollup check` with the defaults and with every port moved.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
REAL_DOCKER="$(command -v docker || true)"   # the stub docker that setup_fixture puts on PATH shadows it

# service  variable  default  container port
TABLE='sequencer RPC_PORT 8545 8545
sequencer WS_PORT 8546 8546
sequencer SEQUENCER_METRICS_PORT 9001 9001
reth-verifier VERIFIER_RPC_PORT 8547 8547
batcher BATCHER_METRICS_PORT 9002 9002
derive DERIVE_METRICS_PORT 9003 9003
prover PROVER_METRICS_PORT 9004 9004'

service_ports() {
  awk -v svc="$1" '
    /^  [a-z0-9-]+:/ { cur=$1; sub(":","",cur); inports=0; next }
    cur==svc && /^    ports:/ { inports=1; next }
    cur==svc && inports && /^      - / { sub(/^      - /,""); gsub(/["'"'"']/,""); sub(/ *#.*/,""); print; next }
    inports && !/^      - / && !/^ *#/ { inports=0 }
  ' "$COMPOSE"
}

# 1. Compose text: each published port is ${VAR:-default}:container, and nothing is a bare number any more.
while read -r svc var def cport; do
  service_ports "$svc" | grep -qE ":\\$\\{$var:-$def\\}:$cport\$" && pass "compose: $svc publishes \${$var:-$def} -> $cport" || fail "compose: $svc publishes \${$var:-$def} -> $cport" "$(service_ports "$svc")"
  grep -qE "^$var=$def( |\$)" "$ROLLUP_DIR/.env.example" && pass ".env.example lists $var=$def" || fail ".env.example lists $var=$def" "missing or a different default"
done <<<"$TABLE"
bare="$(for s in sequencer reth-verifier batcher derive prover postgres; do service_ports "$s"; done | grep -vE '\$\{[A-Z_]+:-[0-9]+\}' || true)"
[[ -z "$bare" ]] && pass "compose: no published port has a fixed host side" || fail "compose: no published port has a fixed host side" "$bare"

# 2. init writes the settings into compose.env, defaults when .env says nothing and the .env value when it does.
setup_fixture
rm -rf "$WORK/out"
if out="$("$ROLLUP" init 2>&1)"; then
  while read -r svc var def cport; do
    grep -qx "$var=$def" "$WORK/out/compose.env" && pass "init: compose.env has $var=$def by default" || fail "init: compose.env has $var=$def by default" "$(grep "^$var=" "$WORK/out/compose.env")"
  done <<<"$TABLE"
  rm -rf "$WORK/out"
  { cat "$WORK/rollup.env"; while read -r svc var def cport; do echo "$var=$((def + 20000))"; done <<<"$TABLE"; } > "$WORK/rollup2.env"
  if ROLLUP_ENV="$WORK/rollup2.env" "$ROLLUP" init >/dev/null 2>&1; then
    while read -r svc var def cport; do
      grep -qx "$var=$((def + 20000))" "$WORK/out/compose.env" && pass "init: compose.env carries the .env value of $var" || fail "init: compose.env carries the .env value of $var" "$(grep "^$var=" "$WORK/out/compose.env")"
    done <<<"$TABLE"
  else fail "init with moved ports" "exited non-zero"; fi
else fail "init with defaults" "$out"; fi

# 3. The rollup refuses a value that is not a port, by name, before touching anything.
out="$(RPC_PORT=eighty "$ROLLUP" init 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^PortInvalid: RPC_PORT' <<<"$out" && pass "a port that is not a number is refused by name" || fail "a port that is not a number is refused by name" "exit=$code $out"
out="$(WS_PORT=70000 "$ROLLUP" init 2>&1)"; code=$?
[[ $code -ne 0 ]] && grep -q '^PortInvalid: WS_PORT' <<<"$out" && pass "a port above 65535 is refused by name" || fail "a port above 65535 is refused by name" "exit=$code $out"

# 4. docker compose config: defaults unchanged, and moving every variable moves every published port.
if [[ -n "$REAL_DOCKER" ]] && "$REAL_DOCKER" compose version >/dev/null 2>&1; then
  published() { # extra env assignments as arguments -> "service published" lines
    local cfg
    cfg="$(cd "$ROLLUP_DIR" && env -u RPC_PORT -u WS_PORT -u VERIFIER_RPC_PORT -u SEQUENCER_METRICS_PORT -u BATCHER_METRICS_PORT -u DERIVE_METRICS_PORT -u PROVER_METRICS_PORT \
      "$@" SEQUENCER_KEY_PATH=/x PAYER_KEYPAIR_PATH=/x CHAIN_ID=1 BLOCK_GAS_LIMIT=1 SOLANA_RPC_URL=x PROVER_DB_PASSWORD_FILE=/x \
      "$REAL_DOCKER" compose --profile prover -f docker-compose.yml config --format json 2>&1)"
    jq -r '.services | to_entries[] | .key as $s | (.value.ports // [])[] | "\($s) \(.target) \(.published)"' <<<"$cfg" | sort
  }
  def_out="$(published ENV_NONE=1)"
  want="$(while read -r svc var def cport; do echo "$svc $cport $def"; done <<<"$TABLE" | sort)"
  [[ "$def_out" == "$want" ]] && pass "compose config: the defaults are 8545 8546 8547 9001 9002 9003 9004, unchanged" || fail "compose config: the defaults are unchanged" "got: $def_out"
  moved_args=(); while read -r svc var def cport; do moved_args+=("$var=$((def + 20000))"); done <<<"$TABLE"
  moved_out="$(published "${moved_args[@]}")"
  want="$(while read -r svc var def cport; do echo "$svc $cport $((def + 20000))"; done <<<"$TABLE" | sort)"
  [[ "$moved_out" == "$want" ]] && pass "compose config: overriding the variables moves every published port" || fail "compose config: overriding the variables moves every published port" "got: $moved_out"
else
  echo "SKIP: compose config checks (docker compose not available; the text checks above ran)"
fi

# 5. ./rollup check talks to the moved ports and to no other.
prepare_check host_ports_from_env
out="$(RPC_PORT=28545 VERIFIER_RPC_PORT=28547 SEQUENCER_METRICS_PORT=29001 BATCHER_METRICS_PORT=29002 "$ROLLUP" check 2>&1)"
calls="$(grep -E '^curl ' "$S/calls" || true)"
grep -q '127.0.0.1:28545' <<<"$calls" && pass "check reads the sequencer on RPC_PORT" || fail "check reads the sequencer on RPC_PORT" "$(head -3 <<<"$calls")"
grep -q '127.0.0.1:28547' <<<"$calls" && pass "check reads the verifier on VERIFIER_RPC_PORT" || fail "check reads the verifier on VERIFIER_RPC_PORT" "$(head -3 <<<"$calls")"
grep -q '127.0.0.1:29001/metrics' <<<"$calls" && pass "check reads sequencer metrics on SEQUENCER_METRICS_PORT" || fail "check reads sequencer metrics on SEQUENCER_METRICS_PORT" "$(head -3 <<<"$calls")"
grep -q '127.0.0.1:29002/metrics' <<<"$calls" && pass "check reads batcher metrics on BATCHER_METRICS_PORT" || fail "check reads batcher metrics on BATCHER_METRICS_PORT" "$(head -3 <<<"$calls")"
grep -qE '127\.0\.0\.1:(8545|8547|9001|9002)' <<<"$calls" && fail "check touches no default port when all are moved" "$(grep -E '127\.0\.0\.1:(8545|8547|9001|9002)' <<<"$calls" | head -2)" || pass "check touches no default port when all are moved"
finish host_ports_from_env
