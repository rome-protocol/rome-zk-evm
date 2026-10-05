#!/usr/bin/env bash
# deploy/rollup/tests/check_keys_path_independent.sh — the key manifest is pinned on the host and checked inside the
# prover container, where the same bytes sit under /opt/zisk. The hash must cover the bytes and the relative file
# names only, never the directory they are mounted at, or every container start refuses with KeysShaMismatch.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
V="$ROLLUP_DIR/prover/check-keys.sh"
mk() { mkdir -p "$1/provingKey/sub" "$1/provingKeySnark"; echo a > "$1/provingKey/f1"; echo b > "$1/provingKey/sub/f3"; echo c > "$1/provingKeySnark/f2"; }
mk "$WORK/hostpath/zisk"; mk "$WORK/ctrpath/opt/zisk"
M="$WORK/m.sha256"
ZISK_HOME="$WORK/hostpath/zisk" MANIFEST="$M" bash "$V" --write >/dev/null 2>&1 && pass "pin under one directory" || fail "pin under one directory" "--write failed"
out="$(ZISK_HOME="$WORK/ctrpath/opt/zisk" MANIFEST="$M" bash "$V" 2>&1)"; rc=$?
[[ $rc == 0 ]] && pass "the same bytes under another directory verify" || fail "the same bytes under another directory verify" "rc=$rc $out"
echo x >> "$WORK/ctrpath/opt/zisk/provingKey/sub/f3"
ZISK_HOME="$WORK/ctrpath/opt/zisk" MANIFEST="$M" bash "$V" >/dev/null 2>&1; [[ $? == 12 ]] && pass "a changed byte under the other directory is KeysShaMismatch" || fail "a changed byte is refused" "accepted"
mk "$WORK/ren/zisk"; mv "$WORK/ren/zisk/provingKey/f1" "$WORK/ren/zisk/provingKey/f9"
ZISK_HOME="$WORK/ren/zisk" MANIFEST="$M" bash "$V" >/dev/null 2>&1; [[ $? == 12 ]] && pass "a renamed key file is KeysShaMismatch" || fail "a renamed key file is refused" "accepted"
# A GPU host writes one *.const_gpu file per circuit under provingKey; the pins come from a CPU host, which writes none.
# The same bytes with such files added (also in a nested directory) must still match, and a changed key file must not.
mk "$WORK/gpu/zisk"; mkdir -p "$WORK/gpu/zisk/provingKey/zisk/Zisk/airs/Arith/air"
echo gen > "$WORK/gpu/zisk/provingKey/zisk/Zisk/airs/Arith/air/Arith.const_gpu"; echo gen > "$WORK/gpu/zisk/provingKey/Main.const_gpu"
ZISK_HOME="$WORK/gpu/zisk" MANIFEST="$M" bash "$V" >/dev/null 2>&1; rc=$?
[[ $rc == 0 ]] && pass "added *.const_gpu files still match the pins" || fail "added *.const_gpu files still match the pins" "rc=$rc (want 0)"
# With aggregation and PLONK, check-setup also writes *.const_gpu, *.consttree and *.consttree_gpu files under
# provingKeySnark (the recursivef circuit). Those are generated too and must not change the hash.
mkdir -p "$WORK/gpu/zisk/provingKeySnark/recursivef"
for x in const_gpu consttree consttree_gpu; do echo gen > "$WORK/gpu/zisk/provingKeySnark/recursivef/recursivef.$x"; done
ZISK_HOME="$WORK/gpu/zisk" MANIFEST="$M" bash "$V" >/dev/null 2>&1; rc=$?
[[ $rc == 0 ]] && pass "generated files under provingKeySnark (*.const_gpu, *.consttree, *.consttree_gpu) still match the pins" || fail "generated files under provingKeySnark still match the pins" "rc=$rc (want 0)"
echo x >> "$WORK/gpu/zisk/provingKey/f1"
ZISK_HOME="$WORK/gpu/zisk" MANIFEST="$M" bash "$V" >/dev/null 2>&1; [[ $? == 12 ]] && pass "a changed key file next to *.const_gpu files is KeysShaMismatch" || fail "a changed key file next to *.const_gpu files is refused" "accepted"
# The sort order must not depend on the locale: the operator pins on a host that may run en_US.UTF-8 (case-insensitive,
# punctuation-ignoring), the container sets no LANG (bytewise). Names that sort differently in the two: Zisk, vadcop_final, zisk.
grep -q 'LC_ALL=C' "$V" && pass "check-keys.sh pins LC_ALL=C itself" || fail "check-keys.sh pins LC_ALL=C itself" "no LC_ALL=C in $V"
mkl() { mkdir -p "$1/provingKey/Zisk" "$1/provingKey/vadcop_final" "$1/provingKey/zisk" "$1/provingKeySnark"; echo 1 > "$1/provingKey/Zisk/b"; echo 2 > "$1/provingKey/vadcop_final/a"; echo 3 > "$1/provingKey/zisk/c"; echo 4 > "$1/provingKey/zisk.d"; echo 5 > "$1/provingKeySnark/s"; }
mkl "$WORK/loc_host/zisk"; mkl "$WORK/loc_ctr/opt/zisk"
ML="$WORK/ml.sha256"
if locale -a 2>/dev/null | grep -qiE '^en_US\.utf-?8$'; then
  ZISK_HOME="$WORK/loc_host/zisk" MANIFEST="$ML" LC_ALL=en_US.UTF-8 bash "$V" --write >/dev/null 2>&1 && pass "pin under LC_ALL=en_US.UTF-8" || fail "pin under LC_ALL=en_US.UTF-8" "--write failed"
  out="$(env -u LANG ZISK_HOME="$WORK/loc_ctr/opt/zisk" MANIFEST="$ML" LC_ALL=C bash "$V" 2>&1)"; rc=$?
  [[ $rc == 0 ]] && pass "a pin made under en_US.UTF-8 verifies under LC_ALL=C" || fail "a pin made under en_US.UTF-8 verifies under LC_ALL=C" "rc=$rc $out"
else
  skip_note="no en_US.UTF-8 locale on this host: the locale case is covered by the LC_ALL=C check above and the reference hash below"
  echo "SKIP: pin under en_US.UTF-8 — $skip_note"
  ZISK_HOME="$WORK/loc_host/zisk" MANIFEST="$ML" bash "$V" --write >/dev/null 2>&1
fi
# The pinned hash is the bytewise recipe, computed here independently under LC_ALL=C.
if command -v sha256sum >/dev/null 2>&1; then H=(sha256sum); else H=(shasum -a 256); fi
ref="$(cd "$WORK/loc_host/zisk/provingKey" && find . -type f -exec "${H[@]}" {} + | LC_ALL=C sort -k2 | "${H[@]}" | awk '{print $1}')"
grep -q "^$ref  provingKey\$" "$ML" && pass "the pinned hash is the bytewise (LC_ALL=C) aggregate" || fail "the pinned hash is the bytewise aggregate" "want $ref, manifest: $(cat "$ML")"
grep -q 'LC_ALL=C' "$ROLLUP_DIR/prover/keys.sha256" && pass "keys.sha256 names the LC_ALL=C recipe" || fail "keys.sha256 names the LC_ALL=C recipe" "not mentioned"
grep -qE 'ZISK_HOME=/opt/zisk .*--write' "$ROLLUP_DIR/README.md" && fail "the README pin command uses the operator's own ZISK_HOME" "it hardcodes /opt/zisk" || pass "the README pin command does not hardcode /opt/zisk"
finish check_keys_path_independent
