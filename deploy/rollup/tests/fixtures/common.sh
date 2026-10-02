# Shared by deploy/rollup/tests/*.sh (sourced, not a test itself: the CI loop only runs tests/*.sh).
# Fixtures + stub tooling only: no network, no cloud credential, no real docker daemon.
ROLLUP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REPO_ROOT="$(cd "$ROLLUP_DIR/../.." && pwd)"
FIX="$ROLLUP_DIR/tests/fixtures"
ROLLUP="$ROLLUP_DIR/rollup"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }
finish() { echo "== $1 :: $( [[ $FAILED -eq 0 ]] && echo ALL PASS || echo SOME FAILED ) =="; exit "$FAILED"; }

# Points the CLI at fixtures and a scratch output directory; nothing under deploy/rollup is written.
setup_fixture() {
  cp "$FIX/chain.toml" "$WORK/chain.toml"
  cp "$FIX/programs.json" "$WORK/programs.json"
  cat > "$WORK/rollup.env" <<ENVEOF
SOLANA_RPC_URL=https://rpc.example.invalid
PROGRAMS_JSON=$WORK/programs.json
SEQUENCER_KEY_PATH=$WORK/keys/sequencer.key
PAYER_KEYPAIR_PATH=$WORK/keys/payer.json
ENVEOF
  mkdir -p "$WORK/keys" "$WORK/initbin"
  echo '[1,2,3]' > "$WORK/keys/payer.json"   # init reads the payer's public key through the stub below; the content is never parsed
  make_chain_id_stub
  export ROLLUP_ENV="$WORK/rollup.env" CHAIN_TOML="$WORK/chain.toml" ROLLUP_OUT="$WORK/out"
}

# A stub `cargo` for the init and register tests. `register_chain chain-id` answers like the real command with a fixed
# authority and the nonce in $STUB_NONCE (default 0); the id is 4295391538 + nonce (a stand-in: the real derivation is
# unit-tested in crates/zk-settlement-client). Every call is appended to $WORK/cargo_calls. Registration reports the
# id of $STUB_REGISTERED_ID when set, otherwise the derived one. $STUB_CHAIN_ID_FAIL makes chain-id fail like an RPC
# error, $STUB_AUTHORITY changes the payer's public key, and init_cursor fails while the file $WORK/init_cursor_fail exists.
make_chain_id_stub() {
  cat > "$WORK/initbin/cargo" <<STUB
#!/usr/bin/env bash
echo "cargo \$*" >> "$WORK/cargo_calls"
nonce="\${STUB_NONCE:-0}"
case " \$* " in
  *" chain-id "*)
    [ -z "\${STUB_CHAIN_ID_FAIL:-}" ] || { echo "NonceLookupFailed: stub RPC error" >&2; exit 1; }
    echo "authority=\${STUB_AUTHORITY:-StubAuthority1111111111111111111111111111111}"
    echo "nonce=\$nonce"
    echo "chain_id=\${STUB_ID:-\$((4295391538 + nonce))}" ;;
  *" --example register_chain "*) echo "chain \${STUB_REGISTERED_ID:-\$((4295391538 + nonce))} registered (permissionless): root StubRoot" ;;
  *" --example init_cursor "*) [ ! -f "$WORK/init_cursor_fail" ] || { echo "stub init_cursor failure" >&2; exit 1; }; echo "cursor initialised" ;;
  *" --example print_pdas "*)
    echo '| batch cursor | zk-inbox | \`StubCursorPda\` |'
    echo '| root | zk-settlement | \`StubRootPda\` |' ;;
  *) exit 1 ;;
esac
STUB
  chmod +x "$WORK/initbin/cargo"
  export PATH="$WORK/initbin:$PATH"
}

# Stub `docker` and `curl` for the register tests, in $WORK/bin (put it on PATH after calling this).
#   curl:   Solana getAccountInfo answers "no such account" until the file $WORK/root_exists exists (but only to a request
#           that says "confirmed" while $WORK/root_confirmed_only exists; or after N reads when $WORK/root_after_n holds N;
#           every read is recorded in $WORK/getinfo_calls), and fails while
#           $WORK/solana_down exists, or for every read after the Nth when $WORK/fail_after_n holds N; any other request is the sequencer's eth_chainId and answers what $WORK/chain_answer
#           holds (a hex id, or "none" for a sequencer that does not answer). curl_stub HEX|none sets that.
#   docker: records every call in $WORK/docker_calls; `up -d sequencer` makes the sequencer answer 0x100067932 when
#           $WORK/start_answers exists.
make_register_stubs() {
  mkdir -p "$WORK/bin"
  cat > "$WORK/bin/docker" <<STUB
#!/bin/sh
echo "docker \$*" >> "$WORK/docker_calls"
if [ -f "$WORK/start_answers" ]; then echo 0x100067932 > "$WORK/chain_answer"; fi
STUB
  cat > "$WORK/bin/curl" <<STUB
#!/bin/sh
case "\$*" in
  *getAccountInfo*)
    [ ! -f "$WORK/solana_down" ] || exit 7
    echo "\$*" >> "$WORK/getinfo_calls"
    n="\$(wc -l < "$WORK/getinfo_calls" | tr -d ' ')"
    seen=0
    if [ -f "$WORK/root_exists" ]; then seen=1; fi
    # A root that landed but is not yet visible at the default commitment: only a request that asks for confirmed sees it.
    if [ -f "$WORK/root_confirmed_only" ]; then case "\$*" in *confirmed*) ;; *) seen=0 ;; esac; fi
    # A request after the Nth read fails on the RPC, as a rate-limited node does.
    if [ -f "$WORK/fail_after_n" ] && [ "\$n" -gt "\$(cat "$WORK/fail_after_n")" ]; then exit 7; fi
    # A root that becomes visible after the Nth read.
    if [ -f "$WORK/root_after_n" ] && [ "\$n" -gt "\$(cat "$WORK/root_after_n")" ]; then seen=1; fi
    if [ "\$seen" = 1 ]; then echo '{"jsonrpc":"2.0","id":1,"result":{"value":{"data":["AAAAAAAAAAAA","base64"]}}}'
    else echo '{"jsonrpc":"2.0","id":1,"result":{"value":null}}'; fi
    exit 0 ;;
esac
a="\$(cat "$WORK/chain_answer")"
[ "\$a" != none ] || exit 7
echo '{"jsonrpc":"2.0","id":1,"result":"'"\$a"'"}'
STUB
  chmod +x "$WORK/bin/docker" "$WORK/bin/curl"
  curl_stub 0x100067932
  export PATH="$WORK/bin:$PATH"
}
curl_stub() { echo "$1" > "$WORK/chain_answer"; }
sends() { grep -c 'example register_chain -- --keypair' "$WORK/cargo_calls" 2>/dev/null || true; }
