#!/usr/bin/env bash
# deploy/rollup/tests/readme_prerequisites.sh — what an outsider on a clean machine needs to know before the first run.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
R="$ROLLUP_DIR/README.md"
has() { tr '\n' ' ' < "$R" | grep -qiE -- "$2" && pass "$1" || fail "$1" "README has no match for: $2"; }
has "the 'You need' list says no Rust is needed" 'you need .{0,300}you do not need rust'
nohas() { tr '\n' ' ' < "$R" | grep -qiE -- "$2" && fail "$1" "README still matches: $2" || pass "$1"; }
nohas "the README no longer asks for rustup or cargo on the host" 'rustup with cargo|cargo run|compiles the client'
has "the README names Docker Engine 28 or newer" 'docker engine 28'
has "the README says ufw does not filter Docker-published ports" 'ufw'
has "the README points at DOCKER-USER or a cloud firewall for RPC_BIND=0.0.0.0" 'DOCKER-USER'
sed -n 2,4p "$ROLLUP" | grep -qiE 'no rust' && pass "the rollup script's own 'You need' line says no Rust is needed" || fail "the rollup script's own 'You need' line says no Rust is needed" "$(sed -n 2,4p "$ROLLUP" | cut -c1-140)"
grep -rniE 'cargo (run|build)' "$ROLLUP" "$R" "$ROLLUP_DIR/../../docs/RUN-ON-DEVNET.md" >/dev/null && fail "no operator doc or script tells the host to run cargo" "found one" || pass "no operator doc or script tells the host to run cargo"
finish readme_prerequisites
