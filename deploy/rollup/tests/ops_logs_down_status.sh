#!/usr/bin/env bash
# deploy/rollup/tests/ops_logs_down_status.sh — `rollup logs`, `down` and `status`. logs and down go to docker compose on
# the rendered compose.env and name a bad argument; status shows the services and what Solana says, through rome-zk-ops from
# the node image, and mounts no key.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_logs_down_status

ops_reset_logs; run_ops "logs" logs
grep -qE 'compose --env-file .*compose.env -f .*docker-compose.yml logs --tail 200$' "$WORK/docker_calls" && pass "logs: docker compose logs, the last 200 lines of every service" || fail "logs: compose call" "$(cat "$WORK/docker_calls")"
ops_reset_logs; run_ops "logs -f --tail 5 sequencer batcher" logs -f --tail 5 sequencer batcher
grep -qE 'logs --tail 5 --follow sequencer batcher$' "$WORK/docker_calls" && pass "logs: --tail, -f and the service names reach compose" || fail "logs: flags" "$(cat "$WORK/docker_calls")"
refuses_by_name UnknownService "$ROLLUP" logs sequencer nonsense
refuses_by_name UnknownArgument "$ROLLUP" logs --since 1h
refuses_by_name FlagValueInvalid "$ROLLUP" logs --tail lots
refuses_by_name FlagValueMissing "$ROLLUP" logs --tail
refuses_by_name NotInitialised env ROLLUP_OUT="$WORK/none" "$ROLLUP" logs

ops_reset_logs; run_ops "down" down
grep -qE 'compose --env-file .*compose.env -f .*docker-compose.yml down$' "$WORK/docker_calls" && pass "down: docker compose down, with no volume flag (the data stays)" || fail "down: compose call" "$(cat "$WORK/docker_calls")"
grep -q -- '-v\b\|--volumes' "$WORK/docker_calls" && fail "down never removes volumes" "$(cat "$WORK/docker_calls")" || pass "down never removes volumes"
refuses_by_name UnknownArgument "$ROLLUP" down --volumes
refuses_by_name UnknownArgument "$ROLLUP" status now

echo "sequencer batcher" > "$WORK/state/running"
ops_reset_logs; run_ops "status" status
grep -q 'chain_id=4295391538' <<<"$OUT_TEXT" && pass "status: names the chain" || fail "status: names the chain" "$OUT_TEXT"
grep -qx 'sequencer' <<<"$OUT_TEXT" && pass "status: lists the running services" || fail "status: lists the services" "$OUT_TEXT"
grep -qxE 'chain-status --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538' "$WORK/ops_calls" && pass "status: reads the chain through rome-zk-ops" || fail "status: chain-status" "$(cat "$WORK/ops_calls")"
grep -q 'source=' "$WORK/ops_mounts" && fail "status: mounts no key" "$(cat "$WORK/ops_mounts")" || pass "status: mounts no key"
touch "$WORK/state/chain_status_fail"
if out="$("$ROLLUP" status 2>&1)"; then fail "status: a chain Solana cannot be read for refuses" "exited 0"
elif grep -q '^StatusFailed' <<<"$out"; then pass "status refuses by name (StatusFailed) when rome-zk-ops cannot read the chain"; else fail "status: StatusFailed" "$out"; fi
finish ops_logs_down_status
