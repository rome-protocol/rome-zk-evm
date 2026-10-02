#!/usr/bin/env bash
# deploy/rollup/tests/register_resumes.sh — `rollup register --confirm` is resumable. If register_chain landed and a later
# step failed, running it again finds the chain's root account for the id init recorded, reports the chain as already
# registered, sends no second registration, and finishes the cursor and pdas.env. A lookup that fails on the RPC stops
# by name instead of guessing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
setup_fixture
ROME_ZK_TAG=test-tag "$ROLLUP" init >/dev/null 2>&1
make_register_stubs
: > "$WORK/cargo_calls"

# The cursor step fails after the registration was sent (the stub's root account now exists on chain).
: > "$WORK/init_cursor_fail"
if out="$("$ROLLUP" register --confirm 2>&1)"; then fail "a failed init_cursor stops register" "exited 0"
elif grep -q '^InitCursorFailed' <<<"$out"; then pass "a failed init_cursor stops register by name (InitCursorFailed)"; else fail "InitCursorFailed" "$out"; fi
[[ "$(sends)" == 1 ]] && pass "the registration was sent once before the failure" || fail "registration sent once" "$(cat "$WORK/cargo_calls")"
[[ ! -e "$ROLLUP_OUT/pdas.env" ]] && pass "no pdas.env after the failure" || fail "no pdas.env after the failure" "found one"

# Re-run: the payer's nonce moved because of our own registration, and the root account exists.
rm -f "$WORK/init_cursor_fail"; : > "$WORK/root_exists"
if out="$(STUB_NONCE=1 "$ROLLUP" register --confirm 2>&1)"; then pass "the re-run finishes after the nonce moved forward"; else fail "the re-run finishes" "$out"; fi
grep -qi 'already registered' <<<"$out" && pass "the re-run says the chain is already registered" || fail "the re-run says the chain is already registered" "$out"
grep -q 'NonceAdvanced' <<<"$out" && fail "the re-run does not stop on NonceAdvanced" "$out" || pass "the re-run does not stop on NonceAdvanced"
[[ "$(sends)" == 1 ]] && pass "exactly one register_chain call in total" || fail "exactly one register_chain call in total" "$(grep -c register_chain "$WORK/cargo_calls") calls"
[[ "$(grep -c 'example init_cursor' "$WORK/cargo_calls")" == 2 ]] && pass "init_cursor ran again" || fail "init_cursor ran again" "$(cat "$WORK/cargo_calls")"
grep -q '^CURSOR_PDA=StubCursorPda$' "$ROLLUP_OUT/pdas.env" && grep -q '^ROOT_PDA=StubRootPda$' "$ROLLUP_OUT/pdas.env" && pass "pdas.env written by the re-run" || fail "pdas.env written by the re-run" "$(cat "$ROLLUP_OUT/pdas.env" 2>/dev/null)"
[[ -z "$(docker_started=$(cat "$WORK/docker_calls" 2>/dev/null); echo "$docker_started" | grep 'up -d' | tail -n +2)" ]] && pass "the re-run did not start the sequencer again" || fail "no second sequencer start" "$(cat "$WORK/docker_calls")"

# A root lookup that fails on the RPC is a named refusal; nothing is sent.
rm -f "$ROLLUP_OUT/pdas.env" "$WORK/root_exists"; : > "$WORK/solana_down"; : > "$WORK/cargo_calls"
if out="$("$ROLLUP" register --confirm 2>&1)"; then fail "an RPC failure on the root lookup refuses" "exited 0"
elif grep -q '^RootLookupFailed' <<<"$out"; then pass "an RPC failure on the root lookup refuses by name (RootLookupFailed)"; else fail "RootLookupFailed" "$out"; fi
[[ "$(sends)" == 0 ]] && pass "nothing was sent when the root lookup failed" || fail "nothing was sent when the root lookup failed" "$(cat "$WORK/cargo_calls")"
# The root is read at the commitment the nonce is read at (confirmed): a root that landed a moment ago is seen.
rm -f "$ROLLUP_OUT/pdas.env" "$WORK/solana_down"; : > "$WORK/root_exists"; : > "$WORK/root_confirmed_only"; : > "$WORK/cargo_calls"; : > "$WORK/getinfo_calls"
if out="$(STUB_NONCE=1 "$ROLLUP" register --confirm 2>&1)"; then pass "a root visible only at confirmed is found by the resume check"; else fail "a root visible only at confirmed is found" "$out"; fi
grep -qi 'already registered' <<<"$out" && pass "...and the chain is reported as already registered" || fail "...and the chain is reported as already registered" "$out"
[[ -s "$WORK/getinfo_calls" ]] && ! grep -v confirmed "$WORK/getinfo_calls" | grep -q . && pass "every getAccountInfo asks for confirmed" || fail "every getAccountInfo asks for confirmed" "$(cat "$WORK/getinfo_calls")"
[[ "$(sends)" == 0 ]] && pass "nothing was sent" || fail "nothing was sent" "$(cat "$WORK/cargo_calls")"

# The nonce moved and the first root read missed it: before advising a move-aside, the root is read again.
rm -f "$ROLLUP_OUT/pdas.env" "$WORK/root_confirmed_only" "$WORK/root_exists"; echo 1 > "$WORK/root_after_n"; : > "$WORK/cargo_calls"; : > "$WORK/getinfo_calls"
if out="$(STUB_NONCE=1 "$ROLLUP" register --confirm 2>&1)"; then pass "a root that shows up on the second read ends the NonceAdvanced stop"; else fail "a root that shows up on the second read" "$out"; fi
grep -q 'NonceAdvanced' <<<"$out" && fail "no NonceAdvanced when the root exists" "$out" || pass "no NonceAdvanced when the root exists on the re-read"
[[ "$(sends)" == 0 ]] && pass "nothing was sent after the re-read" || fail "nothing was sent after the re-read" "$(cat "$WORK/cargo_calls")"
[[ -f "$ROLLUP_OUT/pdas.env" ]] && pass "pdas.env written" || fail "pdas.env written" "missing"
rm -f "$WORK/root_after_n"

# The nonce moved, the first root read found nothing, and the second read fails on the RPC: it is not known whether the chain
# is registered, so the advice is to run register again, never to abandon the output directory.
rm -f "$ROLLUP_OUT/pdas.env" "$WORK/root_exists" "$WORK/root_after_n"; echo 1 > "$WORK/fail_after_n"; : > "$WORK/cargo_calls"; : > "$WORK/getinfo_calls"
if out="$(STUB_NONCE=1 "$ROLLUP" register --confirm 2>&1)"; then fail "a failed second root read refuses" "exited 0"
elif grep -q '^RootLookupFailed' <<<"$out"; then pass "a failed second root read is RootLookupFailed"; else fail "RootLookupFailed on the second read" "$out"; fi
grep -q 'NonceAdvanced' <<<"$out" && fail "a failed second read is not reported as NonceAdvanced" "$out" || pass "a failed second read is not reported as NonceAdvanced"
grep -q 'run register again' <<<"$out" && pass "the refusal says to run register again" || fail "the refusal says to run register again" "$out"
[[ "$(sends)" == 0 ]] && pass "nothing was sent when the second read failed" || fail "nothing was sent when the second read failed" "$(cat "$WORK/cargo_calls")"
rm -f "$WORK/fail_after_n"

# A payer key that is not the authority init recorded is refused by name before any nonce comparison, on both paths.
for state in none exists; do
  rm -f "$ROLLUP_OUT/pdas.env" "$WORK/root_exists"; [[ $state == exists ]] && : > "$WORK/root_exists"; : > "$WORK/cargo_calls"
  if out="$(STUB_AUTHORITY=OtherKey9 STUB_NONCE=5 "$ROLLUP" register --confirm 2>&1)"; then fail "a changed payer key refuses (root $state)" "exited 0"
  elif grep -q '^AuthorityChanged' <<<"$out"; then pass "a changed payer key is AuthorityChanged (root $state)"; else fail "AuthorityChanged (root $state)" "$out"; fi
  grep -q 'NonceAdvanced' <<<"$out" && fail "the changed key is not reported as NonceAdvanced (root $state)" "$out" || pass "the changed key is not reported as NonceAdvanced (root $state)"
  [[ "$(sends)" == 0 && "$(grep -c 'example init_cursor' "$WORK/cargo_calls")" == 0 ]] && pass "nothing was sent and no cursor was written (root $state)" || fail "nothing was sent (root $state)" "$(cat "$WORK/cargo_calls")"
done
rm -f "$WORK/root_exists"
finish register_resumes
