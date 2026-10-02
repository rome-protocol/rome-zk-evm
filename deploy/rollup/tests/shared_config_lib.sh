#!/usr/bin/env bash
# deploy/rollup/tests/shared_config_lib.sh — the config renderers are one library, deploy/rollup/lib/config-lib.sh.
# The rollup CLI sources it and the library defines every function the CLI calls.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
LIB="$ROLLUP_DIR/lib/config-lib.sh"
[[ -f "$LIB" ]] && pass "the shared renderer library exists" || { fail "the shared renderer library exists" "missing $LIB"; finish shared_config_lib; }
grep -q 'config-lib.sh' "$ROLLUP" && pass "rollup sources the shared library" || fail "rollup sources the shared library" "no reference"
for fn in rz_render_template rz_render_sequencer_config rz_profile_gas_limit rz_genesis_diff_summary rz_ensure_jwt; do
  grep -qE "^${fn}\(\)" "$LIB" && pass "library defines $fn" || fail "library defines $fn" "missing"
done
finish shared_config_lib
