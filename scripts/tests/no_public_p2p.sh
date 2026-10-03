#!/usr/bin/env bash
# scripts/tests/no_public_p2p.sh — no Ethereum node we start may join a public peer network. With peer discovery on,
# reth dials hundreds of public Ethereum nodes, which cloud providers flag as cryptocurrency activity. Checked here,
# statically:
#   1. every stock reth or geth service in every deploy/*/docker-compose*.yml starts with --disable-discovery,
#      --max-outbound-peers 0, --max-inbound-peers 0 and its p2p listener on 127.0.0.1 (--addr 127.0.0.1);
#      and no flag appears twice in its command (reth refuses a repeated flag and the node never starts);
#   2. no compose file publishes a devp2p port (30303 or 30304);
#   3. every Rust file that builds an in-process reth node (NodeConfig) disables both discovery kinds.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

found=0
for f in "$ROOT"/deploy/*/docker-compose*.yml; do
  [[ -f "$f" ]] || continue
  rel="${f#"$ROOT"/}"
  svcs="$(awk '/^  [a-z][a-z0-9-]*:$/ { svc=$1; sub(":","",svc) } svc != "" && /image: *[^ ]*(paradigmxyz\/reth|ethereum\/client-go|op-geth)/ { print svc }' "$f" | sort -u)"
  for svc in $svcs; do
    found=1
    block="$(awk -v s="$svc" '/^  [a-z][a-z0-9-]*:$/ { cur=$1; sub(":","",cur) } cur == s' "$f")"
    for flag in "--disable-discovery" "--max-outbound-peers 0" "--max-inbound-peers 0" "--addr 127.0.0.1"; do
      if grep -qF -- "$flag" <<<"$block"; then pass "$rel $svc: $flag"; else fail "$rel $svc: $flag" "missing from its command"; fi
    done
    dups="$(grep -oE -- '--[a-z][a-z0-9.-]*' <<<"$block" | sort | uniq -d | tr '\n' ' ')"
    if [[ -z "$dups" ]]; then pass "$rel $svc: no flag given twice"; else fail "$rel $svc: no flag given twice" "repeated: $dups(reth refuses to start)"; fi
  done
  if grep -nE '^[[:space:]]*-[[:space:]]*"?[^#]*:3030[34]' "$f" >/dev/null; then fail "$rel publishes no devp2p port" "$(grep -nE ':3030[34]' "$f" | head -2 | tr '\n' ' ')"; else pass "$rel publishes no devp2p port"; fi
done
(( found )) || echo "SKIP: no stock reth or geth service in deploy/ (nothing to check)"

while IFS= read -r rs; do
  rel="${rs#"$ROOT"/}"
  for field in "disable_discovery: true" "disable_dns_discovery: true"; do
    if grep -qF -- "$field" "$rs"; then pass "$rel: $field"; else fail "$rel: $field" "an in-process reth node without it"; fi
  done
done < <(grep -rlE 'NodeConfig::(test|new)\(' "$ROOT/crates" "$ROOT/guest" 2>/dev/null | grep '\.rs$')

echo "== no_public_p2p.sh :: $( [[ $FAILED -eq 0 ]] && echo ALL PASS || echo SOME FAILED ) =="
exit "$FAILED"
