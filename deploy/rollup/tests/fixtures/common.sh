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
  make_docker_stub
  export ROME_ZK_IMAGE="${ROME_ZK_IMAGE:-stub/rome-zk-evm:test}"
  export ROLLUP_ENV="$WORK/rollup.env" CHAIN_TOML="$WORK/chain.toml" ROLLUP_OUT="$WORK/out"
}

# A stub `docker` for every test that runs ./rollup. `docker run ... --entrypoint rome-zk-ops IMAGE ARGS` answers like
# rome-zk-ops: the call (the arguments after the image, without --rpc-url) is appended to $WORK/ops_calls, the image to
# $WORK/ops_images and the bind mounts to $WORK/ops_mounts. `chain-id` answers with a fixed authority and the nonce in
# $STUB_NONCE (default 0); the id is 4295391538 + nonce (a stand-in: the real derivation is unit-tested in
# crates/zk-settlement-client). `register` reports the id of $STUB_REGISTERED_ID when set, otherwise the derived one.
# $STUB_CHAIN_ID_FAIL makes chain-id fail like an RPC error, $STUB_AUTHORITY changes the payer's public key (a key file with a `<file>.pub` beside it is that public key instead), and
# `init-cursor` fails while the file $WORK/init_cursor_fail exists. `pdas`, `chain-status`, `exit-config show`, `deposit-queue show`, `vkey show` and `vault show` answer
# from the files under $WORK/state when they exist (see check_stubs.sh), `chain-status` and `exit-config show` fail while
# $WORK/state/chain_status_fail or exit_config_fail exists. Every `docker compose` call is appended to $WORK/docker_calls;
# `compose ps --services --status running` prints $WORK/state/running when that file exists. `up -d sequencer` makes the
# sequencer answer 0x100067932 when $WORK/start_answers exists (the register tests).
make_docker_stub() {
  mkdir -p "$WORK/initbin" "$WORK/state"
  cat > "$WORK/initbin/docker" <<STUB
#!/usr/bin/env bash
W="$WORK"
if [ "\${1:-}" = run ]; then
  shift; image=""; mounts=""
  while [ \$# -gt 0 ]; do
    case "\$1" in
      --rm|--network) [ "\$1" = --network ] && shift; shift ;;
      --mount) mounts="\$mounts \$2"; shift 2 ;;
      --entrypoint) shift 2 ;;
      -*) shift ;;
      *) image="\$1"; shift; break ;;
    esac
  done
  echo "\$image" >> "\$W/ops_images"; echo "\$mounts" >> "\$W/ops_mounts"
  args=()
  while [ \$# -gt 0 ]; do case "\$1" in --rpc-url) shift 2 ;; *) args+=("\$1"); shift ;; esac; done
  echo "\${args[*]}" >> "\$W/ops_calls"
  nonce="\${STUB_NONCE:-0}"
  case " \${args[*]} " in
    *" chain-id "*)
      [ -z "\${STUB_CHAIN_ID_FAIL:-}" ] || { echo "NonceLookupFailed: stub RPC error" >&2; exit 1; }
      # A key file with a sidecar <file>.pub is that public key; any other key is \$STUB_AUTHORITY. A sidecar <file>.fail
      # makes the lookup for that one key fail like an RPC error.
      src="\$(sed -n 's/.*source=\([^,]*\),.*/\1/p' <<<"\$mounts" | head -1)"
      [ -z "\$src" ] || [ ! -f "\$src.fail" ] || { echo "NonceLookupFailed: stub RPC error for this key" >&2; exit 1; }
      if [ -n "\$src" ] && [ -f "\$src.pub" ]; then auth="\$(cat "\$src.pub")"; else auth="\${STUB_AUTHORITY:-StubAuthority1111111111111111111111111111111}"; fi
      echo "authority=\$auth"
      echo "nonce=\$nonce"
      echo "chain_id=\${STUB_ID:-\$((4295391538 + nonce))}" ;;
    *" register "*)
      case " \${args[*]} " in *" --confirm "*) echo "chain \${STUB_REGISTERED_ID:-\$((4295391538 + nonce))} registered (permissionless): root StubRoot" ;; *) echo "  would register chain; pass --confirm to send" ;; esac ;;
    *" init-cursor "*) [ ! -f "\$W/init_cursor_fail" ] || { echo "stub init-cursor failure" >&2; exit 1; }; echo "cursor initialised" ;;
    *" pdas "*) echo "root=StubRootPda"; echo "cursor=StubCursorPda" ;;
    *" chain-status "*)
      [ ! -f "\$W/state/chain_status_fail" ] || { echo "RootLookupFailed: stub RPC error" >&2; exit 1; }
      if [ -f "\$W/state/chain_status" ]; then cat "\$W/state/chain_status"; else
        echo "chain_id=4295391538"; echo "slot=1000"; echo "head_pending_batch=1"; echo "head_final_batch=1"; echo "reclaim_slots_left=none"; echo "vkey_entries=1"; echo "vkey_active=yes"; fi ;;
    *" exit-config show "*)
      [ ! -f "\$W/state/exit_config_fail" ] || { echo "ExitConfigLookupFailed: stub RPC error" >&2; exit 1; }
      echo "exit_config StubEc (chain 4295391538):"; echo "  exit_portal (current)      0x4200000000000000000000000000000000000016"
      if [ -f "\$W/state/exit_config" ]; then cat "\$W/state/exit_config"; else
        echo "  pending_mask                0 (none)"; echo "  activation_slot             n/a (nothing pending)"; fi ;;
    *" deposit-queue show "*)
      [ ! -f "\$W/state/deposit_queue_fail" ] || { echo "DepositQueueLookupFailed: stub RPC error" >&2; exit 1; }
      if [ -f "\$W/state/deposit_queue" ]; then cat "\$W/state/deposit_queue"; else echo "chain_id=4295391538"; echo "queue_exists=false"; fi ;;
    *" vkey show "*)
      [ ! -f "\$W/state/vkey_show_fail" ] || { echo "RegistryLookupFailed: stub RPC error" >&2; exit 1; }
      if [ -f "\$W/state/vkey_show" ]; then cat "\$W/state/vkey_show"; else
        echo "chain_id=4295391538"; echo "slot=1000"; echo "vkey_entries=0"; echo "vkey_active=no"; fi ;;
    *" vault show "*)
      [ ! -f "\$W/state/vault_show_fail" ] || { echo "VaultConfigFetchFailed: stub RPC error" >&2; exit 1; }
      if [ -f "\$W/state/vault_show" ]; then cat "\$W/state/vault_show"; else echo "vault_config StubVault (chain 4295391538): not initialized"; fi ;;
    *) echo "stub rome-zk-ops ok: \${args[*]}" ;;
  esac
  exit 0
fi
echo "docker \$*" >> "\$W/docker_calls"
if [ -f "\$W/start_answers" ]; then echo 0x100067932 > "\$W/chain_answer"; fi
case " \$* " in
  *" ps "*) [ ! -f "\$W/state/running" ] || tr ' ' '\n' < "\$W/state/running" | grep -v '^\$' ;;
esac
exit 0
STUB
  chmod +x "$WORK/initbin/docker"
  export PATH="$WORK/initbin:$PATH"
}

# Stub `docker` and `curl` for the register tests, in $WORK/bin (put it on PATH after calling this).
#   curl:   Solana getAccountInfo answers "no such account" until the file $WORK/root_exists exists (but only to a request
#           that says "confirmed" while $WORK/root_confirmed_only exists; or after N reads when $WORK/root_after_n holds N;
#           every read is recorded in $WORK/getinfo_calls), and fails while
#           $WORK/solana_down exists, or for every read after the Nth when $WORK/fail_after_n holds N; any other request is the sequencer's eth_chainId and answers what $WORK/chain_answer
#           holds (a hex id, or "none" for a sequencer that does not answer). curl_stub HEX|none sets that.
#   docker: the stub above (setup_fixture installs it).
make_register_stubs() {
  mkdir -p "$WORK/bin"
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
  chmod +x "$WORK/bin/curl"
  curl_stub 0x100067932
  export PATH="$WORK/bin:$PATH"
}
curl_stub() { echo "$1" > "$WORK/chain_answer"; }
sends() { grep -c '^register .*--confirm' "$WORK/ops_calls" 2>/dev/null || true; }

# ---- the operator commands (refund-deposit, exit-config, vault, release-exit, migrate, logs, down, status) ----------------
# Every one of them runs rome-zk-ops from the node image through the docker stub above, so a test reads what was run from
# $WORK/ops_calls (the arguments after the image), the bind mounts from $WORK/ops_mounts and the compose calls from
# $WORK/docker_calls. The payer key's content is a marker no output may ever contain.
KEY_MARK="KEYBYTES-MUST-NOT-APPEAR-7731"
prepare_ops() { # $1=test name
  setup_fixture
  echo "$KEY_MARK" > "$WORK/keys/payer.json"
  "$ROLLUP" init >/dev/null 2>&1 || { fail "test setup: rollup init" "failed"; finish "$1"; }
  : > "$WORK/ops_calls"; : > "$WORK/ops_mounts"; : > "$WORK/ops_images"; : > "$WORK/docker_calls"
}
ops_reset_logs() { : > "$WORK/ops_calls"; : > "$WORK/ops_mounts"; : > "$WORK/docker_calls"; }
# run_ops LABEL ARGS... -> runs ./rollup, sets OUT_TEXT, passes when it exits 0; the output joins a log the key check reads
run_ops() {
  local label="$1"; shift
  OUT_TEXT="$("$ROLLUP" "$@" 2>&1)"; RC=$?
  printf '%s\n' "$OUT_TEXT" >> "$WORK/all_output"
  [[ $RC -eq 0 ]] && pass "$label: exits 0" || fail "$label: exits 0" "exit $RC: $OUT_TEXT"
}
# A command that sends is a dry run until --confirm: the call reaches rome-zk-ops without it, and with it ends in --confirm.
# $1=label $2=a pattern the rome-zk-ops call starts with (a grep -E on the whole line) $3..=the ./rollup arguments, without --confirm
dry_run_then_confirm() {
  local label="$1" pat="$2"; shift 2
  ops_reset_logs; run_ops "$label" "$@"
  [[ "$(grep -cE -- "$pat" "$WORK/ops_calls")" == 1 ]] && pass "$label: runs one rome-zk-ops call from the node image" || fail "$label: runs one rome-zk-ops call" "$(cat "$WORK/ops_calls")"
  grep -q -- '--confirm' "$WORK/ops_calls" && fail "$label: sends nothing without --confirm" "$(cat "$WORK/ops_calls")" || pass "$label: sends nothing without --confirm (the call carries no --confirm)"
  grep -qx 'stub/rome-zk-evm:test' "$WORK/ops_images" && pass "$label: runs from the image the .env names" || fail "$label: runs from the image the .env names" "$(cat "$WORK/ops_images")"
  ops_reset_logs
  if out="$("$ROLLUP" "$@" --confirm --dry-run 2>&1)"; then fail "$label: --confirm with --dry-run refuses" "exited 0"
  elif grep -q '^ConfirmAndDryRun' <<<"$out"; then pass "$label: --confirm with --dry-run refuses by name (ConfirmAndDryRun)"; else fail "$label: ConfirmAndDryRun" "$out"; fi
  [[ ! -s "$WORK/ops_calls" ]] && pass "$label: the refusal ran nothing" || fail "$label: the refusal ran nothing" "$(cat "$WORK/ops_calls")"
  # Last, so the logs a caller reads afterwards (mounts, calls) are those of the call that sent.
  ops_reset_logs; run_ops "$label --confirm" "$@" --confirm
  grep -E -- "$pat" "$WORK/ops_calls" | grep -q -- ' --confirm$' && pass "$label: --confirm is passed on to rome-zk-ops" || fail "$label: --confirm is passed on" "$(cat "$WORK/ops_calls")"
}
# $1=label $2..=the host paths of the key files that must be mounted (each once, read-only, at /keys/<name>)
keys_mounted_read_only() {
  local label="$1" host n
  shift
  n=0
  for host in "$@"; do
    grep -q "type=bind,source=$host,target=/keys/[a-z]*,readonly" "$WORK/ops_mounts" && n=$((n+1))
  done
  [[ $n -eq $# ]] && pass "$label: every key is a read-only bind mount by its path" || fail "$label: every key is a read-only bind mount by its path" "$(cat "$WORK/ops_mounts")"
  grep -hE 'source=' "$WORK/ops_mounts" | grep -vq ',readonly$' && fail "$label: no mount is writable" "$(cat "$WORK/ops_mounts")" || pass "$label: no mount is writable"
}
# A key's content is in no output and on no command line.
no_key_content_anywhere() {
  if grep -rqF "$KEY_MARK" "$WORK/all_output" "$WORK/ops_calls" "$WORK/ops_mounts" "$WORK/docker_calls" 2>/dev/null; then fail "$1: a key's content was printed or passed on a command line" "found $KEY_MARK"
  else pass "$1: a key's content is never printed or passed on a command line"; fi
}
# refuses_by_name NAME COMMAND... -> the command exits non-zero, its first line starts with NAME, and rome-zk-ops ran nothing
refuses_by_name() {
  local name="$1" out rc; shift
  ops_reset_logs
  out="$("$@" 2>&1)"; rc=$?
  if [[ $rc -eq 0 ]]; then fail "refuses by name ($name): ${*:2}" "exited 0"
  elif ! grep -q "^$name" <<<"$out"; then fail "refuses by name ($name): ${*:2}" "$out"
  elif [[ -s "$WORK/ops_calls" ]]; then fail "refuses by name ($name): ${*:2}" "rome-zk-ops ran: $(cat "$WORK/ops_calls")"
  else pass "refuses by name ($name): ${*:2}"; fi
}
