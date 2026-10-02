#!/usr/bin/env bash
# deploy/rollup/tests/dockerignore_excludes_local_state.sh — the prover image builds from the repository root. Keys,
# rendered configs (database password, jwt secret), .env and chain.toml sit under deploy/rollup on the operator's
# machine and must never enter the build context. A static check of the root .dockerignore.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
DI="$REPO_ROOT/.dockerignore"
[[ -f "$DI" ]] && pass "the root .dockerignore exists" || { fail "the root .dockerignore exists" "missing"; finish dockerignore_excludes_local_state; }
ignored() { # does some pattern in .dockerignore cover this path? (exact entry, or a directory entry above it)
  local p="$1" line
  while IFS= read -r line; do
    line="${line%%#*}"; line="${line%"${line##*[![:space:]]}"}"; [[ -z "$line" || "$line" == '!'* ]] && continue
    line="${line%/}"
    [[ "$p" == "$line" || "$p" == "$line"/* ]] && return 0
  done < "$DI"
  return 1
}
for p in deploy/rollup/keys deploy/rollup/keys/payer.json deploy/rollup/keys/sequencer.key deploy/rollup/rendered deploy/rollup/rendered/jwt.hex \
         deploy/rollup/.env deploy/rollup/chain.toml deploy/rollup/vkey.json deploy/rollup/elf deploy/rollup/elf/guest.elf; do
  ignored "$p" && pass "$p is outside the build context" || fail "$p is outside the build context" "not covered by .dockerignore"
done
ignored deploy/rollup/prover/entrypoint.sh && fail "the entrypoint the Dockerfile copies stays in the context" "ignored" || pass "the entrypoint the Dockerfile copies stays in the context"
ignored rust-toolchain.toml && fail "rust-toolchain.toml stays in the context" "ignored" || pass "rust-toolchain.toml stays in the context"
finish dockerignore_excludes_local_state
