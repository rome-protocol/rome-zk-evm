#!/usr/bin/env bash
# deploy/rollup/tests/check_items.sh — `rollup check` items against stubbed docker and curl: services, chain id,
# sequencer head (advancing, or idle by its own counter), batcher cursor, inbox batches, roots and settlement lag
# (only meaningful with a prover), derive lag, verifier peers. One world file per knob; every item has a green and a
# red case, and the red ones name what is wrong. The items that read Solana through rome-zk-ops or judge a window
# (batches finalized, verification key, payer floor, reclaim deadline, exit config) are in check_new_items.sh.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/check_stubs.sh"
prepare_check check_items

run() { "$ROLLUP" check 2>&1; }
expect() { # $1=output $2=pattern $3=label
  grep -qE -- "$2" <<<"$1" && pass "$3" || fail "$3" "no line /$2/ in: $(grep -E '^(PASS|FAIL|SKIP)' <<<"$1" | head -12 | tr '\n' '|')"
}

out="$(run)"; code=$?
[[ $code -eq 0 ]] && pass "a healthy world: check exits 0" || fail "a healthy world: check exits 0" "exit $code: $out"
for svc in sequencer batcher reth-verifier derive; do expect "$out" "^PASS: service $svc" "service $svc running"; done
expect "$out" '^PASS: chain id' "chain id matches chain.toml"
expect "$out" '^PASS: sequencer head' "sequencer head advances"
expect "$out" '^PASS: batcher cursor .*next_batch=7' "batcher cursor decoded"
expect "$out" '^SKIP: batches finalized — no sample from at least' "batches finalized has no baseline on a first run"
expect "$out" '^PASS: inbox batches .*6' "inbox batches posted (next_batch - 1)"
expect "$out" '^PASS: roots .*head_final_batch=5' "root decoded"
expect "$out" '^SKIP: settlement lag — no prover' "settlement lag is skipped without a prover"
expect "$out" '^PASS: derive lag' "derive follows the sequencer"
expect "$out" '^PASS: verifier peers' "the verifier has no peers (discovery off)"

echo 0x1 > "$S/chain_id_hex"; out="$(run)"
expect "$out" '^FAIL: chain id — ChainIdMismatch' "wrong chain id fails by name"; echo 0x100067932 > "$S/chain_id_hex"

echo 0 > "$S/seq_step"; echo 0 > "$S/idle_step"; out="$(run)"
expect "$out" '^FAIL: sequencer head — SequencerStalled' "no new block and no idle tick fails by name"
echo 1 > "$S/idle_step"; out="$(run)"
expect "$out" '^PASS: sequencer head .*idle' "an idle chain (ticks, no blocks) is healthy"
echo 1 > "$S/seq_step"; echo 0 > "$S/idle_step"

echo missing > "$S/cursor_next"; out="$(run)"
expect "$out" '^FAIL: batcher cursor — CursorUnreadable' "an absent cursor account fails by name"
echo 7 > "$S/cursor_next"
rm "$ROLLUP_OUT/pdas.env"; out="$(run)"
expect "$out" '^FAIL: batcher cursor — PdasMissing' "no pdas.env fails by name and says how to get it"
expect "$out" 'rollup register' "the PdasMissing line points at rollup register"
printf 'CURSOR_PDA=CursorPdaFixture111111111111111111111111112\nROOT_PDA=RootPdaFixture1111111111111111111111111111112\n' > "$ROLLUP_OUT/pdas.env"

# Inbox batches judge the sequencer's head against what the posted batches could hold (blocks_per_batch = 30 in the fixture).
echo 1 > "$S/cursor_next"; echo 10 > "$S/seq_block"; out="$(run)"
expect "$out" '^PASS: inbox batches .*0 posted' "nothing posted yet is fine while the sequencer is under two batches"
echo 100 > "$S/seq_block"; out="$(run)"
expect "$out" '^FAIL: inbox batches — NoBatchPosted' "nothing posted and the sequencer two batches ahead fails by name"
echo 2 > "$S/cursor_next"; echo 100 > "$S/seq_block"; out="$(run)"
expect "$out" '^FAIL: inbox batches — BatcherBehind' "a posted batch that cannot hold the sequencer's blocks fails by name"
echo 7 > "$S/cursor_next"; echo 120 > "$S/seq_block"

echo 40 > "$S/ver_block"; out="$(run)"
expect "$out" '^FAIL: derive lag — DeriveBehind' "a verifier far behind the sequencer fails by name"
echo 95 > "$S/ver_block"
echo 1 > "$S/ver_step"; out="$(run)"
expect "$out" '^PASS: derive lag' "a verifier that keeps pace is fine"
echo 0 > "$S/ver_step"

echo 3 > "$S/ver_peers"; out="$(run)"
expect "$out" '^FAIL: verifier peers — VerifierHasPeers' "a verifier with peers fails by name (it must never join a public peer network)"
echo 0 > "$S/ver_peers"

# With a prover configured: settlement lag and prover freshness become real items.
echo "PROVER=on" >> "$ROLLUP_ENV"; echo "sequencer batcher reth-verifier derive prover postgres" > "$S/running"
out="$(run)"
expect "$out" '^PASS: settlement lag' "settlement lag within bound (next_batch 7, final 5 -> 1)"
expect "$out" '^PASS: prover lag' "prover behind by 0 batches"
echo 2 > "$S/root_final"; out="$(run)"
expect "$out" '^FAIL: settlement lag — .*lag=4' "settlement lag over the bound fails with the numbers"
echo 5 > "$S/root_final"
echo 9 > "$S/prover_behind"; out="$(run)"
expect "$out" '^FAIL: prover lag' "a prover many batches behind fails"
echo 0 > "$S/prover_behind"; echo 7468 > "$S/prover_lag"; out="$(run)"
expect "$out" '^PASS: prover lag' "a caught-up prover on a quiet chain passes, whatever age its last proved batch had"
echo 1 > "$S/prover_behind"; out="$(run)"
expect "$out" '^FAIL: prover lag — ProverBehind' "an unproved batch older than 600 s fails by name"
echo 0 > "$S/prover_lag"
echo "sequencer batcher reth-verifier derive" > "$S/running"; out="$(run)"
expect "$out" '^FAIL: service prover — ServiceMissing' "PROVER=on with no prover running is named"
finish check_items
