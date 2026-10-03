#!/usr/bin/env bash
# deploy/rollup/tests/prover_compose_memlock_and_gpu.sh — the prover service asks for what cargo-zisk needs at run time:
# an unlimited locked-memory limit (cargo-zisk refuses to run under the default one) and an NVIDIA GPU. Without the GPU
# reservation Docker starts the container with no GPU and the prover would stop at its own GPU check, or, with
# gpu = false, prove on the CPU for hours without anyone choosing it.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
[[ -f "$COMPOSE" ]] || { fail "compose file exists" "missing"; finish prover_compose_memlock_and_gpu; }

block="$(awk '/^  [a-z][a-z0-9-]*:$/ { cur=$1; sub(":","",cur) } cur == "prover"' "$COMPOSE")"
[[ -n "$block" ]] || { fail "the compose file has a prover service" "none found"; finish prover_compose_memlock_and_gpu; }

# memlock: ulimits -> memlock -> soft: -1 and hard: -1 (both, on their own lines, inside the memlock mapping).
mem="$(awk '/^    ulimits:/ {u=1; next} u && /^    [a-z]/ {u=0} u' <<<"$block")"
if grep -qE '^      memlock:' <<<"$mem"; then pass "the prover has a memlock ulimit"; else fail "the prover has a memlock ulimit" "no ulimits.memlock"; fi
for k in soft hard; do
  if grep -qE "^        $k: *-1\$" <<<"$mem"; then pass "memlock $k is -1 (unlimited)"; else fail "memlock $k is -1 (unlimited)" "not set to -1"; fi
done

# GPU: deploy.resources.reservations.devices with the nvidia driver and the gpu capability.
dev="$(awk '/^          devices:/ {d=1; next} d && /^    [a-z]/ {d=0} d' <<<"$block")"
if grep -qE 'driver: *nvidia' <<<"$dev"; then pass "the prover reserves an NVIDIA device"; else fail "the prover reserves an NVIDIA device" "no devices entry with driver: nvidia"; fi
if grep -qE 'capabilities: *\[ *gpu *\]' <<<"$dev"; then pass "the device reservation asks for the gpu capability"; else fail "the device reservation asks for the gpu capability" "no capabilities: [gpu]"; fi
if grep -qE 'count: *(all|[1-9])' <<<"$dev"; then pass "the device reservation names a count"; else fail "the device reservation names a count" "no count"; fi

# The two settings belong to the prover only: the node services must not reserve a GPU.
others="$(awk '/^  [a-z][a-z0-9-]*:$/ { cur=$1; sub(":","",cur) } cur != "prover"' "$COMPOSE")"
if grep -qE 'driver: *nvidia' <<<"$others"; then fail "only the prover reserves a GPU" "another service has an nvidia device"; else pass "only the prover reserves a GPU"; fi

# The prover config the container reads says gpu = true, so the prover refuses a CPU-only cargo-zisk by name.
grep -qE '^gpu *= *true' "$ROLLUP_DIR/prover.toml.template" && pass "the rendered prover config says gpu = true" || fail "the rendered prover config says gpu = true" "prover.toml.template has no gpu = true"
finish prover_compose_memlock_and_gpu
