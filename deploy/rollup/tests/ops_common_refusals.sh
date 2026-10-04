#!/usr/bin/env bash
# deploy/rollup/tests/ops_common_refusals.sh — what every operator command that talks to Solana refuses by name before it
# runs anything: no init yet, no image or tag, a payer key that is not there.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_common_refusals
HASH="0x$(printf 'c3%.0s' $(seq 32))"
echo '[1]' > "$WORK/keys/registry.json"

# Each line is one command, written as the arguments after ./rollup.
CMDS=(
  "refund-deposit"
  "exit-config propose --exit-cap 5 --activation-slot 9"
  "exit-config activate"
  "exit-config show"
  "vault init"
  "vault fund --amount 5"
  "vault show"
  "release-exit --message-hash $HASH"
  "migrate --registry-keypair $WORK/keys/registry.json --max-drift-secs 60"
  "status"
)
for c in "${CMDS[@]}"; do
  # shellcheck disable=SC2086
  refuses_by_name NotInitialised env ROLLUP_OUT="$WORK/never-rendered" "$ROLLUP" $c
done

# No image: strip it from what init rendered and from the environment.
cp "$WORK/out/compose.env" "$WORK/compose.env.kept"
grep -v '^ROME_ZK_IMAGE=\|^ROME_ZK_TAG=' "$WORK/compose.env.kept" > "$WORK/out/compose.env"
for c in "${CMDS[@]}"; do
  # shellcheck disable=SC2086
  refuses_by_name ImageTagNotSet env -u ROME_ZK_IMAGE "$ROLLUP" $c
done
cp "$WORK/compose.env.kept" "$WORK/out/compose.env"

# A payer key that is not there: the commands that sign refuse by name, the reads do not need one.
mv "$WORK/keys/payer.json" "$WORK/keys/payer.gone"
for c in "refund-deposit" "exit-config propose --exit-cap 5 --activation-slot 9" "exit-config activate" "vault init" "vault fund --amount 5" "release-exit --message-hash $HASH" "migrate --registry-keypair $WORK/keys/registry.json --max-drift-secs 60"; do
  # shellcheck disable=SC2086
  refuses_by_name KeyFileMissing "$ROLLUP" $c
done
for c in "exit-config show" "vault show"; do
  ops_reset_logs
  # shellcheck disable=SC2086
  "$ROLLUP" $c >/dev/null 2>&1 && pass "$c needs no key file" || fail "$c needs no key file" "refused"
done
finish ops_common_refusals
