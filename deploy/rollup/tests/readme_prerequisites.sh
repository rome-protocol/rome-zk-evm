#!/usr/bin/env bash
# deploy/rollup/tests/readme_prerequisites.sh — what an outsider on a clean machine needs to know before the first run.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
R="$ROLLUP_DIR/README.md"
has() { tr '\n' ' ' < "$R" | grep -qiE -- "$2" && pass "$1" || fail "$1" "README has no match for: $2"; }
has "the 'You need' list names rustup or cargo" 'you need .{0,200}rustup'
has "the README says the first init compiles the client" 'first `?(\./rollup )?init`? (compiles|builds)'
has "the README names Docker Engine 28 or newer" 'docker engine 28'
has "the README says ufw does not filter Docker-published ports" 'ufw'
has "the README points at DOCKER-USER or a cloud firewall for RPC_BIND=0.0.0.0" 'DOCKER-USER'
sed -n 2,3p "$ROLLUP" | grep -qiE 'rustup|cargo' && pass "the rollup script's own 'You need' line names rustup or cargo" || fail "the rollup script's own 'You need' line names rustup or cargo" "$(sed -n 2,3p "$ROLLUP" | cut -c1-140)"
finish readme_prerequisites
