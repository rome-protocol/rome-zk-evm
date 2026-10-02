#!/usr/bin/env bash
# deploy/rollup/tests/no_cloud_cli_referenced.sh — grep-driven. The portable deploy runs on any machine with docker:
# no cloud command-line tool, no hosted key store, no managed database, no monitoring stack, and none of Rome's own
# names, hosts or shorthand. Applies to this directory, its tests, and the shared renderer library.
# This directory is published, so no line under it may hold the words it scans for, this file included: every
# pattern is built from fragments at run time (j joins its arguments).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
j() { local IFS=; echo "$*"; }

mapfile -t FILES < <(cd "$ROLLUP_DIR" && find . -type f ! -path './rendered/*' | sort)
mapfile -t DEPLOY < <(printf '%s\n' "${FILES[@]}" | grep -v '^./tests/')
[[ ${#DEPLOY[@]} -ge 8 ]] && pass "found the deploy files to scan (${#DEPLOY[@]})" || fail "found the deploy files to scan" "only ${#DEPLOY[@]} files under deploy/rollup"
[[ ${#FILES[@]} -gt ${#DEPLOY[@]} ]] && pass "the tests are scanned too (${#FILES[@]} files in all)" || fail "the tests are scanned too" "no test files found"

scan() { # $1=label $2=egrep pattern $3=optional "deploy": scan only the deploy files, not the tests
  local hits files=("${FILES[@]}")
  [[ "${3:-}" == deploy ]] && files=("${DEPLOY[@]}")
  hits="$(cd "$ROLLUP_DIR" && grep -nEi -- "$2" "${files[@]}" 2>/dev/null | head -5)"
  [[ -z "$hits" ]] && pass "no $1" || fail "no $1" "$hits"
}
CLI="$(j g cloud)"; STORE="$(j g sm)"; SQL="$(j cloud ' ' sql)"; NAME="$(j ti ber)"
scan "cloud command-line tool" "(^|[^a-z])($CLI|$(j g sutil))([^a-z]|\$)"
scan "hosted key store" "(^|[^a-z])$STORE([^a-z]|\$)|$(j secret ' ?' manager)|$(j secret man ager)|$(j secrets ' ' versions)"
scan "managed SQL database or its proxy" "$(j cloud '[ -]?' sql)"
scan "a cloud tunnel or cloud ssh" "$(j tunnel -through-i ap)|$(j compute ' ' ssh)"
scan "cloud project, zone or registry hosts" "$(j gcr '\\.' io)|$(j google apis)|$(j GCP_ PROJECT)|$(j GCP_ ZONE)|$(j rome -developers)"
scan "Rome internal names, hosts or shorthand" "(^|[^a-z])($NAME|$(j tx v1)|$(j ledger ' [0-9]')|$(j TRACK ER))([^a-z]|\$)|$(j romeprotocol '\\.' xyz)|$(j zk-c i-runner)"
scan "a monitoring stack image" 'prom/|grafana/|node-exporter|prometheus\.yml|alertmanager' deploy

# No monitoring or alert files either.
bad="$(cd "$ROLLUP_DIR" && find . -type f ! -path './tests/*' \( -iname '*prometheus*' -o -iname '*grafana*' -o -iname '*alert*' -o -iname '*node-exporter*' -o -iname '*dashboard*' \) | head -3)"
[[ -z "$bad" ]] && pass "no Prometheus, Grafana, node-exporter, dashboard or alert files" || fail "no monitoring files" "$bad"

# The CLI works with docker and curl on PATH and nothing cloud-specific: stub those two, put a cloud tool on PATH
# that fails loudly, and run init.
setup_fixture
mkdir -p "$WORK/bin"; printf '#!/bin/sh\necho "cloud CLI was called" >&2\nexit 99\n' > "$WORK/bin/$CLI"; chmod +x "$WORK/bin/$CLI"
cp "$WORK/bin/$CLI" "$WORK/bin/$(j g sutil)"
if out="$(PATH="$WORK/bin:$PATH" "$ROLLUP" init 2>&1)" && ! grep -q 'cloud CLI was called' <<<"$out"; then pass "rollup init never calls a cloud CLI"; else fail "rollup init never calls a cloud CLI" "$out"; fi
finish no_cloud_cli_referenced
