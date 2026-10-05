#!/usr/bin/env bash
# deploy/rollup/tests/exit_check_items.sh — the `rollup check` items for withdrawals. Without EXITS=on they are skipped with
# the reason. With it: the exit-prover service must run, its metrics must answer, the chain's exit config must be active
# (the exit prover scans only then) and no exit may be parked as stuck. The exit prover's own payer must hold its floor,
# and its log scan must not be failing between two checks. Each failure names what is wrong and the fix.
# docker and curl are stubs; the exit prover's metrics come from $S/exit_metrics and the exit payer's balance from
# $S/exit_payer_lamports.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
prepare_check exit_check_items

# The exit prover's metrics endpoint, on top of the shared curl stub: 9005 answers the file $S/exit_metrics, or nothing
# when the file is absent (a service that is down).
EXIT_PUB="ExitPayerStub1111111111111111111111111111111"
mv "$WORK/stubbin/curl" "$WORK/stubbin/curl-base"
cat > "$WORK/stubbin/curl" <<STUB
#!/usr/bin/env bash
case "\$*" in
  *127.0.0.1:9005/metrics*) [ -f "$S/exit_metrics" ] || exit 7; cat "$S/exit_metrics" ;;
  *getBalance*$EXIT_PUB*)
    v="\$(cat "$S/exit_payer_lamports")"; [ "\$v" = missing ] && exit 7
    printf '{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":%s}}\n' "\$v" ;;
  *) exec "$WORK/stubbin/curl-base" "\$@" ;;
esac
STUB
chmod +x "$WORK/stubbin/curl"
echo 500000000 > "$S/exit_payer_lamports"

run() { "$ROLLUP" check 2>&1; }
expect() { grep -qE -- "$2" <<<"$1" && pass "$3" || fail "$3" "no line /$2/ in: $(grep -E '^(PASS|FAIL|SKIP|WARN)' <<<"$1" | grep -i exit | head -8 | tr '\n' '|')"; }
healthy_metrics() { printf '# TYPE rome_zk_exit_active gauge\nrome_zk_exit_active 1\nrome_zk_exit_gate_read_ok 1\nrome_zk_exit_pending 0\nrome_zk_exits_proved_total 3\nrome_zk_exit_rpc_errors_total{method="eth_getLogs"} 4\n' > "$S/exit_metrics"; }
log_errors() { printf 'rome_zk_exit_active 1\nrome_zk_exit_gate_read_ok 1\nrome_zk_exit_rpc_errors_total{method="eth_getLogs"} %s\nrome_zk_exit_rpc_errors_total{method="eth_blockNumber"} %s\n' "$1" "${2:-0}" > "$S/exit_metrics"; }

# Off.
out="$(run)"
expect "$out" '^SKIP: exit prover — no exit prover configured \(EXITS=on in .env turns it on\)' "without EXITS=on the exit prover item is skipped with the reason"
expect "$out" '^SKIP: stuck exits — no exit prover configured' "without EXITS=on the stuck-exits item is skipped with the reason"
expect "$out" '^SKIP: exits active — no exit prover configured' "without EXITS=on the exits-active item is skipped with the reason"
expect "$out" '^SKIP: exit payer balance — no exit prover configured' "without EXITS=on the exit payer balance item is skipped with the reason"
expect "$out" '^SKIP: exit log scan — no exit prover configured' "without EXITS=on the exit log scan item is skipped with the reason"
grep -q 'service exit-prover' <<<"$out" && fail "no service item for the exit prover when it is off" "$out" || pass "no service item for the exit prover when it is off"

# On and healthy.
echo '[4,5,6]' > "$WORK/keys/exit-payer.json"; echo "$EXIT_PUB" > "$WORK/keys/exit-payer.json.pub"
echo "EXITS=on" >> "$ROLLUP_ENV"; echo "EXIT_PAYER_KEYPAIR_PATH=$WORK/keys/exit-payer.json" >> "$ROLLUP_ENV"; "$ROLLUP" init >/dev/null 2>&1
echo "sequencer batcher reth-verifier derive exit-prover" > "$S/running"; healthy_metrics
out="$(run)"; code=$?
expect "$out" '^PASS: service exit-prover' "the exit-prover service is checked when it is on"
expect "$out" '^PASS: exit prover — answering on 127.0.0.1:9005' "metrics answering passes"
expect "$out" '^PASS: exits active — ' "an active exit config passes"
expect "$out" '^PASS: stuck exits — none' "no stuck exits passes"
expect "$out" '^PASS: exit payer balance — 500000000 lamports at ExitPayerStub' "a funded exit payer passes, with its balance and key"
expect "$out" '^SKIP: exit log scan — no earlier sample' "the first check has no earlier sample for the log scan"
[[ $code -eq 0 ]] && pass "a healthy exit prover: check exits 0" || fail "a healthy exit prover: check exits 0" "exit $code"

# Failures, by name.
echo "sequencer batcher reth-verifier derive" > "$S/running"; out="$(run)"; code=$?
expect "$out" '^FAIL: service exit-prover — ServiceMissing: exit-prover is not running \(start it with ./rollup up\)' "a stopped exit prover fails by name"
expect "$out" '^SKIP: exit prover — needs exit-prover, which is not running' "its metrics item says why it is skipped"
expect "$out" '^SKIP: exit log scan — needs exit-prover, which is not running' "so does the log scan"
expect "$out" '^PASS: exit payer balance' "the exit payer's balance is still read while the service is down"
[[ $code -ne 0 ]] && pass "a stopped exit prover makes check exit non-zero" || fail "a stopped exit prover makes check exit non-zero" "exit 0"
echo "sequencer batcher reth-verifier derive exit-prover" > "$S/running"

rm "$S/exit_metrics"; out="$(run)"
expect "$out" '^FAIL: exit prover — ExitProverUnreachable: no answer from http://127.0.0.1:9005/metrics \(./rollup logs exit-prover\)' "a running service with no metrics fails by name"

printf 'rome_zk_exit_active 0\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^FAIL: exits active — ExitsNotActive: the chain.s exit config names no portal or its exit cap is 0, so the exit prover waits \(./rollup exit-config propose, then activate\)' "an inactive exit config fails by name, with the fix"

printf 'rome_zk_exit_active 1\nrome_zk_exit_stuck{reason="proof_too_large"} 1\nrome_zk_exit_stuck{reason="max_send_attempts"} 2\nrome_zk_exit_stuck{reason="exceeds_window_cap"} 0\n' > "$S/exit_metrics"
out="$(run)"; code=$?
expect "$out" '^FAIL: stuck exits — ExitsStuck: 3 exits are parked as stuck \(max_send_attempts=2 proof_too_large=1\); see ./rollup logs exit-prover' "stuck exits fail by name with the counts per reason"
[[ $code -ne 0 ]] && pass "stuck exits make check exit non-zero" || fail "stuck exits make check exit non-zero" "exit 0"
printf 'rome_zk_exit_active 1\nrome_zk_exit_stuck{reason="proof_too_large"} 0\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^PASS: stuck exits — none' "series at zero are not stuck exits"

printf 'rome_zk_exit_pending 0\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^FAIL: exits active — ExitsNotActive' "a metrics page without the active gauge is not read as active"

# The exit payer's balance: below the floor fails by name; the floor moves with EXIT_PAYER_FLOOR_LAMPORTS.
healthy_metrics
echo 99999999 > "$S/exit_payer_lamports"; out="$(run)"; code=$?
expect "$out" '^FAIL: exit payer balance — ExitPayerBelowFloor: 99999999 lamports at ExitPayerStub[A-Za-z0-9]*, floor 100000000 ' "an exit payer under 100,000,000 lamports fails by name"
[[ $code -ne 0 ]] && pass "a low exit payer makes check exit non-zero" || fail "a low exit payer makes check exit non-zero" "exit 0"
echo 100000000 > "$S/exit_payer_lamports"; out="$(run)"
expect "$out" '^PASS: exit payer balance — 100000000 lamports' "exactly the default floor passes"
echo 100000000 > "$S/exit_payer_lamports"; out="$(EXIT_PAYER_FLOOR_LAMPORTS=250000000 "$ROLLUP" check 2>&1)"
expect "$out" '^FAIL: exit payer balance — ExitPayerBelowFloor: 100000000 lamports .*, floor 250000000 ' "EXIT_PAYER_FLOOR_LAMPORTS moves the floor"
echo missing > "$S/exit_payer_lamports"; out="$(run)"
expect "$out" '^FAIL: exit payer balance — ExitPayerBalanceUnreadable' "an unreadable balance is not read as funded"
echo 500000000 > "$S/exit_payer_lamports"

# The log scan: the error counter for eth_getLogs, compared with the previous check's sample.
rm -f "$ROLLUP_OUT"/check-samples*
log_errors 4; out="$(run)"
expect "$out" '^SKIP: exit log scan — no earlier sample' "the first sample is recorded and not judged"
log_errors 4; out="$(run)"; code=$?
expect "$out" '^PASS: exit log scan — no new log scan errors' "an unchanged error counter passes"
log_errors 6; out="$(run)"; code=$?
expect "$out" '^FAIL: exit log scan — ExitLogScanFailing: log scan errors .* grew by 2 since the last check' "a growing error counter fails by name"
expect "$out" 'ExitLogScanFailing.*(verifier|logs exit-prover)' "the failure points at the verifier node and the exit prover's logs"
[[ $code -ne 0 ]] && pass "a failing log scan makes check exit non-zero" || fail "a failing log scan makes check exit non-zero" "exit 0"
log_errors 6; out="$(run)"
expect "$out" '^PASS: exit log scan' "once the counter stops growing the item passes again"
# The scan reads the verifier's head first; when that fails, no eth_getLogs call is made at all.
log_errors 6 3; out="$(run)"
expect "$out" '^FAIL: exit log scan — ExitLogScanFailing' "a growing eth_blockNumber error counter fails too"
log_errors 6 3; out="$(run)"
log_errors 1; out="$(run)"
expect "$out" '^FAIL: exit log scan — ExitLogScanFailing: log scan errors .* grew by 1' "after a restart (counter below the last sample) every error counts as new"
printf 'rome_zk_exit_active 1\nrome_zk_exit_gate_read_ok 1\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^PASS: exit log scan' "a page with no error series counts as zero errors"
rm "$S/exit_metrics"; out="$(run)"
expect "$out" '^SKIP: exit log scan — the exit prover does not answer' "no answer, no judgement"
healthy_metrics

# Exits active: when the exit prover could not read the chain's exit config or root, say that, not ExitsNotActive.
printf 'rome_zk_exit_active 0\nrome_zk_exit_gate_read_ok 0\n' > "$S/exit_metrics"; out="$(run)"; code=$?
expect "$out" '^FAIL: exits active — ExitConfigUnreadableByExitProver: the exit prover could not read the chain.s exit config or root from Solana' "an unreadable gate is named, not reported as not active"
expect "$out" 'ExitConfigUnreadableByExitProver.*SOLANA_RPC_URL' "the fix is the Solana RPC the exit prover uses"
grep -q 'ExitsNotActive' <<<"$out" && fail "unreadable is not ExitsNotActive" "$(grep 'exits active' <<<"$out")" || pass "unreadable is not ExitsNotActive"
[[ $code -ne 0 ]] && pass "an unreadable gate makes check exit non-zero" || fail "an unreadable gate makes check exit non-zero" "exit 0"
printf 'rome_zk_exit_active 0\nrome_zk_exit_gate_read_ok 1\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^FAIL: exits active — ExitsNotActive' "a gate that read fine and is inactive is still ExitsNotActive"
printf 'rome_zk_exit_active 1\nrome_zk_exit_gate_read_ok 1\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^PASS: exits active' "a gate that read fine and is active passes"
printf 'rome_zk_exit_active 1\n' > "$S/exit_metrics"; out="$(run)"
expect "$out" '^PASS: exits active' "a page from a build without the read gauge is judged by the active gauge"
finish exit_check_items
