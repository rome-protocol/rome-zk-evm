#!/usr/bin/env bash
# deploy/rollup/tests/ops_migrate.sh — `rollup migrate` runs rome-zk-ops migrate from the node image. It is a dry run until
# --confirm, mounts the payer key and the registry authority's key read-only by their paths and never prints either.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
prepare_ops ops_migrate
PAYER="$WORK/keys/payer.json"
REG="$WORK/keys/registry.json"
echo "$KEY_MARK-registry" > "$REG"

dry_run_then_confirm "migrate" '^migrate --settlement FixtureBprogram11111111111111111111111111112 --chain-id 4295391538 --registry-keypair /keys/registry --payer-keypair /keys/payer --max-drift-secs 60' migrate --registry-keypair "$REG" --max-drift-secs 60
keys_mounted_read_only "migrate" "$PAYER" "$REG"
no_key_content_anywhere "migrate"

refuses_by_name RegistryKeypairMissing "$ROLLUP" migrate --max-drift-secs 60
refuses_by_name MaxDriftMissing "$ROLLUP" migrate --registry-keypair "$REG"
refuses_by_name FlagValueMissing "$ROLLUP" migrate --registry-keypair
refuses_by_name FlagValueMissing "$ROLLUP" migrate --registry-keypair "$REG" --max-drift-secs
refuses_by_name FlagValueInvalid "$ROLLUP" migrate --registry-keypair "$REG" --max-drift-secs soon
refuses_by_name KeyFileMissing "$ROLLUP" migrate --registry-keypair "$WORK/keys/none.json" --max-drift-secs 60
refuses_by_name KeyPathInvalid "$ROLLUP" migrate --registry-keypair "$WORK/keys/a,b.json" --max-drift-secs 60
refuses_by_name UnknownArgument "$ROLLUP" migrate --registry-keypair "$REG" --max-drift-secs 60 --force
finish ops_migrate
