#!/usr/bin/env bash
# scripts/tests/check_workspace_deps.sh — proves scripts/check-workspace-deps.sh refuses every manifest
# shape that can carry a Solana version pin, and stays quiet on a clean tree.
#
# One throwaway git repo per case (the check lists manifests with `git ls-files`), each holding a root
# Cargo.toml with a [workspace.dependencies] table and one member manifest. Shapes covered:
#   - inline          solana-x = "=..."                               -> LiteralSolanaPin
#   - table           [dependencies.solana-x] version = "=..."        -> LiteralSolanaPin
#   - rename          foo = { package = "solana-x", version = ... }   -> LiteralSolanaPin
#   - dev + target    [target.'cfg(unix)'.dev-dependencies]           -> LiteralSolanaPin
#   - spl / agave     the same refusal for spl-* and agave-* crates
#   - excluded crate  a standalone crate on a different version        -> ExcludedCrateOutOfLockstep
# and the passing shapes: workspace = true in every form, an excluded crate on the same version, a
# non-Solana crate with its own version. Last, the real tree must pass.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHECK="${CHECK_CMD:-$REPO_ROOT/scripts/check-workspace-deps.sh}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

ROOT_TOML='[workspace]
members = ["crates/member"]
exclude = ["crates/rome-zk-prover"]

[workspace.dependencies]
solana-program = "=4.1.0"
solana-sdk = "=4.1.0"
solana-client = "=4.3.0"
spl-token = "=9.0.0"
agave-feature-set = "=4.3.0"
'

# new_repo <name> <member Cargo.toml body> [excluded-crate Cargo.toml body]
new_repo() {
  local d="$WORK/$1"
  mkdir -p "$d/crates/member"
  printf '%s' "$ROOT_TOML" > "$d/Cargo.toml"
  printf '%s\n' "$2" > "$d/crates/member/Cargo.toml"
  if [[ $# -ge 3 ]]; then
    mkdir -p "$d/crates/rome-zk-prover"
    printf '%s\n' "$3" > "$d/crates/rome-zk-prover/Cargo.toml"
  fi
  (cd "$d" && git init -q . && git add -A) >/dev/null
  echo "$d"
}

# expect_red <label> <repo> <ErrorName> <text that must appear in the output>
expect_red() {
  local out code
  out="$("$CHECK" --root "$2" 2>&1)"; code=$?
  if [[ $code -ne 0 ]] && grep -q "$3" <<<"$out" && grep -q -- "$4" <<<"$out"; then
    pass "$1 -> $3"
  else
    fail "$1 -> $3" "exit=$code out=$out"
  fi
}
expect_green() {
  local out code
  out="$("$CHECK" --root "$2" 2>&1)"; code=$?
  if [[ $code -eq 0 ]]; then pass "$1 passes"; else fail "$1 passes" "exit=$code out=$out"; fi
}

HEAD='[package]
name = "member"
version = "0.0.0"
edition = "2021"
'

# --- the refusals ---
expect_red "inline pin" "$(new_repo inline "$HEAD
[dependencies]
solana-program = \"=4.1.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:solana-program"

expect_red "table-form pin" "$(new_repo table "$HEAD
[dependencies.solana-client]
version = \"=4.3.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:solana-client"

expect_red "renamed import" "$(new_repo rename "$HEAD
[dependencies]
sdk4 = { package = \"solana-sdk\", version = \"=4.1.0\" }")" LiteralSolanaPin "crates/member/Cargo.toml:sdk4"

expect_red "dev-dependency pin" "$(new_repo devdep "$HEAD
[dev-dependencies]
solana-sdk = \"=4.1.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:solana-sdk"

expect_red "build-dependency pin" "$(new_repo builddep "$HEAD
[build-dependencies]
solana-sdk = \"=4.1.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:solana-sdk"

expect_red "target-specific dev-dependency pin" "$(new_repo target "$HEAD
[target.'cfg(unix)'.dev-dependencies]
solana-sdk = { version = \"=4.1.0\", features = [\"full\"] }")" LiteralSolanaPin "crates/member/Cargo.toml:solana-sdk"

expect_red "target-specific table-form pin" "$(new_repo targettable "$HEAD
[target.'cfg(unix)'.dependencies.solana-client]
version = \"=4.3.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:solana-client"

expect_red "spl pin" "$(new_repo spl "$HEAD
[dependencies]
spl-token = \"=9.0.0\"")" LiteralSolanaPin "crates/member/Cargo.toml:spl-token"

expect_red "agave pin" "$(new_repo agave "$HEAD
[dependencies]
agave-feature-set = { version = \"=4.3.0\" }")" LiteralSolanaPin "crates/member/Cargo.toml:agave-feature-set"

expect_red "excluded crate out of lockstep (inline)" "$(new_repo lockinline "$HEAD" "$HEAD
[dependencies]
solana-program = \"=2.1.6\"")" ExcludedCrateOutOfLockstep "crates/rome-zk-prover/Cargo.toml:solana-program"

expect_red "excluded crate out of lockstep (table form)" "$(new_repo locktable "$HEAD" "$HEAD
[dependencies.solana-client]
version = \"=4.2.1\"")" ExcludedCrateOutOfLockstep "crates/rome-zk-prover/Cargo.toml:solana-client"

expect_red "excluded crate out of lockstep (renamed)" "$(new_repo lockrename "$HEAD" "$HEAD
[dependencies]
sdk4 = { package = \"solana-sdk\", version = \"=4.0.0\" }")" ExcludedCrateOutOfLockstep "crates/rome-zk-prover/Cargo.toml:sdk4"

expect_red "excluded crate with a Solana crate the workspace does not list" "$(new_repo lockunknown "$HEAD" "$HEAD
[dependencies]
solana-unlisted = \"=1.0.0\"")" ExcludedCrateOutOfLockstep "crates/rome-zk-prover/Cargo.toml:solana-unlisted"

# --- the passes ---
expect_green "workspace = true in every shape" "$(new_repo clean "$HEAD
[dependencies]
solana-program.workspace = true
spl-token = { workspace = true }
sdk4 = { workspace = true, package = \"solana-sdk\" }
[dependencies.solana-client]
workspace = true
[dev-dependencies]
solana-sdk = { workspace = true, features = [\"full\"] }
[target.'cfg(unix)'.dev-dependencies]
agave-feature-set.workspace = true
serde = \"=1.0.0\"")"

expect_green "excluded crate on the same versions" "$(new_repo lockstep "$HEAD" "$HEAD
[dependencies]
solana-program = \"=4.1.0\"
sdk4 = { package = \"solana-sdk\", version = \"=4.1.0\" }
[dependencies.solana-client]
version = \"=4.3.0\"
serde = \"=1.0.0\"")"

# --- the real tree ---
expect_green "the real tree" "$REPO_ROOT"

echo "== check_workspace_deps.sh :: $( [[ $FAILED -eq 0 ]] && echo ALL PASS || echo SOME FAILED ) =="
exit "$FAILED"
