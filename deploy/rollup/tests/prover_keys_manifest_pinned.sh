#!/usr/bin/env bash
# deploy/rollup/tests/prover_keys_manifest_pinned.sh — the shipped keys.sha256 is pinned to a real key set, never the
# placeholder, so check-keys.sh passes without --write on a host that holds that key set and refuses any other by name.
# The test cannot hash the real keys (about 81 GB); it checks the manifest's shape and runs check-keys.sh against a tiny
# fixture to show that a pinned manifest of this shape is what a plain run accepts and a changed key is refused.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
M="$ROLLUP_DIR/prover/keys.sha256"
V="$ROLLUP_DIR/prover/check-keys.sh"
[[ -f "$M" ]] || { fail "keys.sha256 exists" "missing"; finish prover_keys_manifest_pinned; }

for d in provingKey provingKeySnark; do
  if grep -qE "^[0-9a-f]{64}  $d\$" "$M"; then pass "$d is pinned to a sha256"; else fail "$d is pinned to a sha256" "no '<64 hex>  $d' line in keys.sha256"; fi
done
if grep -q 'UNPINNED' <(grep -v '^#' "$M"); then fail "no placeholder is left in the manifest" "UNPINNED found"; else pass "no placeholder is left in the manifest"; fi
n="$(grep -cvE '^(#|$)' "$M")"
[[ "$n" == 2 ]] && pass "the manifest has exactly the two key directories" || fail "the manifest has exactly the two key directories" "$n entries"
if grep -E '^[0-9a-f]{64}  provingKey$' "$M" | cut -c1-64 | grep -qxF "$(grep -E '^[0-9a-f]{64}  provingKeySnark$' "$M" | cut -c1-64)"; then fail "the two pins differ" "identical hashes"; else pass "the two pins differ"; fi

# A fixture key set pinned with --write passes a plain run; a changed byte is KeysShaMismatch; the shipped manifest
# refuses a fixture that is not Rome's key set.
mkdir -p "$WORK/z/provingKey" "$WORK/z/provingKeySnark"; echo a > "$WORK/z/provingKey/k1"; echo b > "$WORK/z/provingKeySnark/k2"
cp "$M" "$WORK/own.sha256"
ZISK_HOME="$WORK/z" MANIFEST="$WORK/own.sha256" bash "$V" --write >/dev/null 2>&1
grep -qE '^[0-9a-f]{64}  provingKey$' "$WORK/own.sha256" && ZISK_HOME="$WORK/z" MANIFEST="$WORK/own.sha256" bash "$V" >/dev/null 2>&1 \
  && pass "a pinned manifest passes a plain run, no --write" || fail "a pinned manifest passes a plain run, no --write" "refused"
echo x >> "$WORK/z/provingKey/k1"
ZISK_HOME="$WORK/z" MANIFEST="$WORK/own.sha256" bash "$V" >/dev/null 2>&1; [[ $? == 12 ]] && pass "a changed key file is KeysShaMismatch" || fail "a changed key file is KeysShaMismatch" "accepted"
ZISK_HOME="$WORK/z" MANIFEST="$M" bash "$V" >/dev/null 2>&1; [[ $? == 12 ]] && pass "the shipped pin refuses a key set that is not Rome's" || fail "the shipped pin refuses a key set that is not Rome's" "wrong exit"
# The manifest names no host path and no cloud, so it is safe in the public tree.
grep -qE '/(home|srv|opt|Users)/' <(grep -v '^#' "$M") && fail "the manifest holds no host path" "path found" || pass "the manifest holds no host path"
finish prover_keys_manifest_pinned
