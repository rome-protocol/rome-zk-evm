# Sourced by the check tests. Stubs `curl` on PATH (the shared `docker` stub is in common.sh and reads $S/running too); every answer comes from a file under $S, so a test
# sets the world by writing files. `jq` and `python3` are the real ones. Nothing here touches a network.
S="$WORK/state"; mkdir -p "$S" "$WORK/stubbin"

set_world() { # healthy by default; callers overwrite single files
  echo "sequencer batcher reth-verifier derive" > "$S/running"
  echo 0x100067932 > "$S/chain_id_hex"   # 4295391538, what the init stub derives
  echo 100 > "$S/seq_block"; echo 1 > "$S/seq_step"
  echo 95 > "$S/ver_block"; echo 0 > "$S/ver_step"
  echo 500 > "$S/idle_ticks"; echo 0 > "$S/idle_step"
  echo 3 > "$S/oldest_age"
  echo 7 > "$S/cursor_next"; echo 5 > "$S/root_final"
  echo 0 > "$S/prover_behind"; echo 12 > "$S/prover_lag"
  echo 0 > "$S/ver_peers"
  echo 700 > "$S/sealed_total"; echo 0 > "$S/sealed_step"; echo 40 > "$S/finalized_total"; echo 0 > "$S/finalized_step"
  echo 5000000000 > "$S/payer_lamports"
  rm -f "$S/chain_status" "$S/chain_status_fail" "$S/exit_config_fail"
}

make_stubs() {
cat > "$WORK/stubbin/curl" <<STUB
#!/usr/bin/env bash
url=""; body=""
while [[ \$# -gt 0 ]]; do case "\$1" in -d|--data) body="\$2"; shift 2;; http*) url="\$1"; shift;; *) shift;; esac; done
echo "curl \$url \$body" >> "$S/calls"
S="$S"
bump() { v=\$(cat "\$S/\$1"); echo \$(( v + \$(cat "\$S/\$2") )) > "\$S/\$1"; echo "\$v"; }
case "\$url" in
  http://127.0.0.1:8545|http://127.0.0.1:8545/)
    case "\$(jq -r .method <<<"\$body")" in
      eth_chainId) printf '{"jsonrpc":"2.0","id":1,"result":"%s"}\n' "\$(cat "\$S/chain_id_hex")" ;;
      eth_blockNumber) printf '{"jsonrpc":"2.0","id":1,"result":"0x%x"}\n' "\$(bump seq_block seq_step)" ;;
      *) exit 1 ;;
    esac ;;
  http://127.0.0.1:8547|http://127.0.0.1:8547/)
    case "\$(jq -r .method <<<"\$body")" in
      net_peerCount) printf '{"jsonrpc":"2.0","id":1,"result":"0x%x"}\n' "\$(cat "\$S/ver_peers")" ;;
      *) printf '{"jsonrpc":"2.0","id":1,"result":"0x%x"}\n' "\$(bump ver_block ver_step)" ;;
    esac ;;
  http://127.0.0.1:9001/metrics) printf '# TYPE x counter\nrome_zk_sequencer_idle_ticks_total %s\nrome_zk_sequencer_blocks_sealed_total %s\n' "\$(bump idle_ticks idle_step)" "\$(bump sealed_total sealed_step)" ;;
  http://127.0.0.1:9002/metrics) printf 'rome_zk_batcher_oldest_unposted_block_age_seconds %s\nrome_zk_batcher_batches_finalized_total %s\n' "\$(cat "\$S/oldest_age")" "\$(bump finalized_total finalized_step)" ;;
  http://127.0.0.1:9004/metrics) printf 'rome_zk_prover_batches_behind %s\nrome_zk_prover_lag_seconds %s\n' "\$(cat "\$S/prover_behind")" "\$(cat "\$S/prover_lag")" ;;
  https://rpc.example.invalid*)
    pk="\$(jq -r '.params[0]' <<<"\$body")"
    if [[ "\$(jq -r .method <<<"\$body")" == getBalance ]]; then
      v="\$(cat "\$S/payer_lamports")"; [[ "\$v" == missing ]] && exit 7
      printf '{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":%s}}\n' "\$v"; exit 0
    fi
    case "\$pk" in
      CursorPdaFixture*) kind=cursor; val="\$(cat "\$S/cursor_next")" ;;
      RootPdaFixture*) kind=root; val="\$(cat "\$S/root_final")" ;;
      *) exit 1 ;;
    esac
    [[ "\$val" == missing ]] && { echo '{"jsonrpc":"2.0","id":1,"result":{"value":null}}'; exit 0; }
    python3 - "\$kind" "\$val" <<'PY'
import sys, base64, struct
kind, val = sys.argv[1], int(sys.argv[2])
length, off = (21, 13) if kind == "cursor" else (202, 186)
d = bytearray(length); struct.pack_into("<Q", d, off, val)
print('{"jsonrpc":"2.0","id":1,"result":{"value":{"data":["%s","base64"]}}}' % base64.b64encode(bytes(d)).decode())
PY
    ;;
  *) exit 1 ;;
esac
STUB
chmod +x "$WORK/stubbin/curl"
export PATH="$WORK/stubbin:$PATH"
}

# init renders compose.env; the PDAs are what `rollup register` writes. Fixture names make the stub's dispatch obvious.
prepare_check() {
  setup_fixture
  "$ROLLUP" init >/dev/null 2>&1 || { fail "test setup: rollup init" "failed"; finish "$1"; }
  printf 'CURSOR_PDA=CursorPdaFixture111111111111111111111111112\nROOT_PDA=RootPdaFixture1111111111111111111111111111112\n' > "$ROLLUP_OUT/pdas.env"
  set_world; make_stubs
  export CHECK_SAMPLE_SECS=0   # the real check samples 3 s apart; the stub moves on every call
}
