#!/usr/bin/env bash
# The prover image's entrypoint: check the proving keys against the manifest, then start the prover. It runs on
# every container start, including a restart after a crash, so a changed or half-downloaded key set never proves.
# The compose file mounts check-keys.sh and keys.sha256 read-only at the paths below.
set -euo pipefail

VERIFY_SCRIPT="${VERIFY_SCRIPT:-/opt/prover/check-keys.sh}"
MANIFEST="${MANIFEST:-/opt/prover/keys.sha256}"
: "${ZISK_HOME:?ZISK_HOME must be set}"

if [[ ! -f "$VERIFY_SCRIPT" ]]; then
  echo "VerifyScriptMissing: $VERIFY_SCRIPT not found; mount check-keys.sh there (docker-compose.yml does)" >&2
  exit 1
fi
if [[ ! -f "$MANIFEST" ]]; then
  echo "KeysManifestMissing: $MANIFEST not found; mount keys.sha256 there (docker-compose.yml does)" >&2
  exit 1
fi

MANIFEST="$MANIFEST" bash "$VERIFY_SCRIPT"

exec rome-zk-prover "$@"
