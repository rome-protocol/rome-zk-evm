#!/usr/bin/env bash
# deploy/rollup/tests/up_and_images.sh — what an outsider can pull or build. The node image is a pinned tag (never
# `main`), `./rollup up` refuses by name while no tag is set, and the prover service builds from this tree, mounts
# what its own entrypoint requires, and relies on no file from the private deploy tree.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
COMPOSE="$ROLLUP_DIR/docker-compose.yml"
PROVER_DIR="$ROLLUP_DIR/prover"
REAL_DOCKER="$(command -v docker || true)"   # the stub docker below shadows it

# 1. Image references.
grep -nE '^\s*image:' "$COMPOSE" | grep -qE ':main\b|:latest\b' && fail "no floating image tag in the compose file" "$(grep -nE ':main|:latest' "$COMPOSE")" || pass "no floating image tag (main or latest) in the compose file"
grep -qE 'rome-zk-evm:\$\{ROME_ZK_TAG' "$COMPOSE" && pass "the node image is the public package with a tag variable" || fail "the node image is the public package with a tag variable" "$(grep -n 'image:' "$COMPOSE" | head -3)"
grep -q 'ROME_ZK_TAG' "$ROLLUP_DIR/.env.example" && pass ".env.example documents ROME_ZK_TAG" || fail ".env.example documents ROME_ZK_TAG" "missing"

# 2. `up` refuses by name without a tag, and starts with one.
setup_fixture
mkdir -p "$WORK/bin"; printf '#!/bin/sh\necho "docker $*" >> "%s/docker_calls"\n' "$WORK" > "$WORK/bin/docker"; chmod +x "$WORK/bin/docker"
printf 'ab%.0s' $(seq 32) > "$WORK/keys/sequencer.key"
export PATH="$WORK/bin:$PATH"
"$ROLLUP" init >/dev/null 2>&1
: > "$WORK/docker_calls"
if out="$("$ROLLUP" up sequencer 2>&1)"; then fail "up refuses without an image tag" "exited 0"
elif grep -q '^ImageTagNotSet' <<<"$out"; then pass "up refuses by name (ImageTagNotSet) while no tag is set"; else fail "up refuses by name (ImageTagNotSet)" "$out"; fi
[[ ! -s "$WORK/docker_calls" ]] && pass "nothing was started without a tag" || fail "nothing was started without a tag" "$(cat "$WORK/docker_calls")"
rm -rf "$WORK/out"; ROME_ZK_TAG=abc123 "$ROLLUP" init >/dev/null 2>&1
grep -q '^ROME_ZK_TAG=abc123$' "$WORK/out/compose.env" && pass "init passes ROME_ZK_TAG to compose.env" || fail "init passes ROME_ZK_TAG to compose.env" "$(cat "$WORK/out/compose.env")"
if out="$("$ROLLUP" up sequencer 2>&1)" && grep -q 'compose .*up -d sequencer' "$WORK/docker_calls"; then pass "up starts with a tag set"; else fail "up starts with a tag set" "$out"; fi

# 3. The prover: built from this tree, mounts what its entrypoint needs.
for f in Dockerfile entrypoint.sh check-keys.sh keys.sha256; do [[ -f "$PROVER_DIR/$f" ]] && pass "deploy/rollup/prover/$f exists" || fail "deploy/rollup/prover/$f exists" "missing"; done
grep -q 'deploy/rollup/prover/entrypoint.sh' "$PROVER_DIR/Dockerfile" && pass "the Dockerfile copies the entrypoint from deploy/rollup/prover" || fail "the Dockerfile copies the entrypoint from deploy/rollup/prover" "$(grep -n COPY "$PROVER_DIR/Dockerfile")"
# The pattern is built from fragments so this published file holds no private path itself.
PRIV='deploy/pro''ver/|deploy/ti''ber'
! grep -nE "$PRIV" "$PROVER_DIR/Dockerfile" "$PROVER_DIR/entrypoint.sh" "$COMPOSE" >/dev/null && pass "nothing in the prover build or compose names the private deploy tree" || fail "no reference to the private deploy tree" "$(grep -nE "$PRIV" "$PROVER_DIR/Dockerfile" "$PROVER_DIR/entrypoint.sh" "$COMPOSE")"
# The entrypoint's required paths, from its own defaults, against the compose mount targets.
want_verify="$(sed -n 's/^VERIFY_SCRIPT="\${VERIFY_SCRIPT:-\(.*\)}"/\1/p' "$PROVER_DIR/entrypoint.sh")"
want_manifest="$(sed -n 's/^MANIFEST="\${MANIFEST:-\(.*\)}"/\1/p' "$PROVER_DIR/entrypoint.sh")"
[[ -n "$want_verify" && -n "$want_manifest" ]] && pass "the entrypoint requires $want_verify and $want_manifest" || fail "the entrypoint names its required files" "verify='$want_verify' manifest='$want_manifest'"
mounts="$(awk '/^  prover:/{p=1;next} /^  [a-z0-9-]+:/{p=0} p&&/^      - /{print}' "$COMPOSE")"
grep -qF ":$want_verify:ro" <<<"$mounts" && pass "the prover service mounts the key-verify script at $want_verify" || fail "the prover service mounts the key-verify script" "$mounts"
grep -qF ":$want_manifest:ro" <<<"$mounts" && pass "the prover service mounts the manifest at $want_manifest" || fail "the prover service mounts the manifest" "$mounts"
grep -qE '^      - \./prover/check-keys\.sh:' <<<"$mounts" && pass "the mounted key-verify script is the one shipped in deploy/rollup/prover" || fail "the mounted key-verify script is the shipped one" "$mounts"
awk '/^  prover:/{p=1;next} /^  [a-z0-9-]+:/{p=0} p' "$COMPOSE" | grep -q 'dockerfile: deploy/rollup/prover/Dockerfile' && pass "the prover service builds from deploy/rollup/prover/Dockerfile" || fail "the prover service has a build section" "missing"

# The same, from the resolved config, when docker compose is available.
if [[ -n "$REAL_DOCKER" ]] && "$REAL_DOCKER" compose version >/dev/null 2>&1; then
  cfg="$(cd "$ROLLUP_DIR" && SEQUENCER_KEY_PATH=/x PAYER_KEYPAIR_PATH=/x CHAIN_ID=1 BLOCK_GAS_LIMIT=1 PROVER_DB_PASSWORD_FILE=/x "$REAL_DOCKER" compose --profile prover -f docker-compose.yml config --format json 2>&1)"
  if jq -e --arg v "$want_verify" --arg m "$want_manifest" '[.services.prover.volumes[].target] | (index($v) != null and index($m) != null)' >/dev/null 2>&1 <<<"$cfg"; then pass "compose config: the prover mounts $want_verify and $want_manifest"; else fail "compose config: the prover mounts what its entrypoint requires" "$(head -c 300 <<<"$cfg")"; fi
  jq -e '.services.prover.build.dockerfile == "deploy/rollup/prover/Dockerfile"' >/dev/null 2>&1 <<<"$cfg" && pass "compose config: the prover builds from deploy/rollup/prover/Dockerfile" || fail "compose config: the prover build" "missing"
  ctx="$(jq -r '.services.prover.build.context' <<<"$cfg" 2>/dev/null)"; [[ -f "$ctx/deploy/rollup/prover/Dockerfile" && -f "$ctx/rust-toolchain.toml" ]] && pass "compose config: the build context holds the Dockerfile and rust-toolchain.toml" || fail "the build context is the repository root" "context=$ctx"
else
  echo "SKIP: compose config checks for the prover (docker compose not available; the text checks above ran)"
fi

# 4. The shipped verify script refuses the shipped placeholder manifest by name and accepts a pinned copy.
KH="$WORK/zisk"; mkdir -p "$KH/provingKey" "$KH/provingKeySnark"; echo a > "$KH/provingKey/f1"; echo b > "$KH/provingKeySnark/f2"
out="$(ZISK_HOME="$KH" bash "$PROVER_DIR/check-keys.sh" 2>&1)"; rc=$?
[[ $rc == 11 ]] && grep -q KeysManifestUnpinned <<<"$out" && pass "the shipped manifest is a placeholder: KeysManifestUnpinned, exit 11" || fail "the shipped manifest is refused by name" "rc=$rc $out"
cp "$PROVER_DIR/keys.sha256" "$WORK/keys.sha256"
ZISK_HOME="$KH" MANIFEST="$WORK/keys.sha256" bash "$PROVER_DIR/check-keys.sh" --write >/dev/null 2>&1
ZISK_HOME="$KH" MANIFEST="$WORK/keys.sha256" bash "$PROVER_DIR/check-keys.sh" >/dev/null 2>&1 && pass "a manifest pinned with --write verifies" || fail "a pinned manifest verifies" "refused"
echo c >> "$KH/provingKey/f1"
ZISK_HOME="$KH" MANIFEST="$WORK/keys.sha256" bash "$PROVER_DIR/check-keys.sh" >/dev/null 2>&1; [[ $? == 12 ]] && pass "a changed key file is KeysShaMismatch (exit 12)" || fail "a changed key file is refused" "accepted"
# The entrypoint itself refuses without the mounted files.
out="$(ZISK_HOME="$KH" VERIFY_SCRIPT="$WORK/none.sh" bash "$PROVER_DIR/entrypoint.sh" 2>&1)"; grep -q '^VerifyScriptMissing' <<<"$out" && pass "the entrypoint refuses by name without the verify script" || fail "the entrypoint refuses without the verify script" "$out"
out="$(ZISK_HOME="$KH" VERIFY_SCRIPT="$PROVER_DIR/check-keys.sh" MANIFEST="$WORK/none.sha256" bash "$PROVER_DIR/entrypoint.sh" 2>&1)"; grep -q '^KeysManifestMissing' <<<"$out" && pass "the entrypoint refuses by name without the manifest" || fail "the entrypoint refuses without the manifest" "$out"
finish up_and_images
