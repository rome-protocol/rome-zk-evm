#!/usr/bin/env bash
# deploy/rollup/tests/guest_build_lock_check.sh — the check the guest-build image runs right after it clones the guest
# (deploy/rollup/guest-build/check-guest-lock.sh): the guest's Cargo.lock must pin ziskos and every other zisk-* crate at the
# image's ZisK release, or the build stops by name. A guest built on the crates of one release and compiled with the toolchain
# of another would be labelled with a release it was not built for. Fixtures only: no network, no docker.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
CHECK="$ROLLUP_DIR/guest-build/check-guest-lock.sh"
[[ -x "$CHECK" ]] && pass "the lock check is an executable script of the image" || { fail "the lock check exists and is executable" "$CHECK"; finish guest_build_lock_check; }

# A trimmed Cargo.lock in the shape cargo writes: other crates, then the ZisK crates, each at the version given.
lock() { # $1=file; ziskos version, then one version per zisk-* crate
  local f="$1" zv="$2"; shift 2
  {
    echo 'version = 4'; echo
    printf '[[package]]\nname = "alloy-evm"\nversion = "0.33.3"\nsource = "git+https://github.com/0xPolygonHermez/zisk-patch-alloy-evm.git?branch=zisk-hints%%2Fv0.33.3#abc"\n\n'
    printf '[[package]]\nname = "serde"\nversion = "1.0.200"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n\n'
    local n=0 c
    for c in zisk-circuit zisk-definitions zisk-lib-c zisk-stream; do
      printf '[[package]]\nname = "%s"\nversion = "%s"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n\n' "$c" "${1:-$zv}"; n=$((n+1))
    done
    [[ "$zv" == none ]] || printf '[[package]]\nname = "ziskos"\nversion = "%s"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\ndependencies = [\n "zisk-definitions",\n]\n\n' "$zv"
  } > "$f"
}
run() { OUT="$("$CHECK" "$@" 2>&1)"; RC=$?; }
refused() { [[ $RC -ne 0 ]] && grep -q "^$1" <<<"$OUT" && pass "$2: refused by name ($1)" || fail "$2: refused by name ($1)" "rc=$RC $OUT"; }

lock "$WORK/ok.lock" 1.3.1-alpha
run 1.3.1-alpha "$WORK/ok.lock"
[[ $RC -eq 0 ]] && pass "a lock file with ziskos and every zisk-* crate at the image's release is accepted" || fail "a matching lock file is accepted" "rc=$RC $OUT"

# The v0.2.0 guest was built on ziskos 1.2.0-alpha: in the 1.3.1-alpha image it is refused, and the refusal says why.
lock "$WORK/old.lock" 1.2.0-alpha
run 1.3.1-alpha "$WORK/old.lock"
refused GuestZiskMismatch "a guest locked to ZisK 1.2.0-alpha in the 1.3.1-alpha image"
grep -q 'ziskos' <<<"$OUT" && grep -q '1\.2\.0-alpha' <<<"$OUT" && grep -q '1\.3\.1-alpha' <<<"$OUT" && pass "the refusal names the crate, the version the guest has and the release of the image" || fail "the refusal names crate and both versions" "$OUT"
run 1.2.0-alpha "$WORK/old.lock"
[[ $RC -eq 0 ]] && pass "the same guest is accepted in the 1.2.0-alpha image" || fail "the 1.2.0-alpha guest in the 1.2.0-alpha image" "rc=$RC $OUT"

# ziskos right, one other zisk-* crate at another release.
lock "$WORK/mixed.lock" 1.3.1-alpha 1.2.0-alpha
run 1.3.1-alpha "$WORK/mixed.lock"
refused GuestZiskMismatch "ziskos at the release but another zisk-* crate not"
grep -q 'zisk-circuit' <<<"$OUT" && pass "the refusal names the other zisk-* crate" || fail "the refusal names zisk-circuit" "$OUT"

# ziskos not in the lock file at all.
lock "$WORK/nozisk.lock" none 1.3.1-alpha
run 1.3.1-alpha "$WORK/nozisk.lock"
refused GuestZiskosMissing "a lock file with no ziskos"

# Not a lock file; missing; no release.
run 1.3.1-alpha "$WORK/does-not-exist.lock"; refused GuestLockMissing "no Cargo.lock in the guest"
: > "$WORK/empty.lock"; run 1.3.1-alpha "$WORK/empty.lock"; refused GuestZiskosMissing "an empty lock file"
run "" "$WORK/ok.lock"; refused ZiskReleaseUnknown "an empty release"
run; refused ZiskReleaseUnknown "no arguments"

# A crate that only looks like a ZisK crate is not one.
{ cat "$WORK/ok.lock"; printf '[[package]]\nname = "ziskos-extras"\nversion = "9.9.9"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n\n[[package]]\nname = "not-zisk-lib"\nversion = "0.0.1"\n'; } > "$WORK/lookalike.lock"
run 1.3.1-alpha "$WORK/lookalike.lock"
[[ $RC -eq 0 ]] && pass "crates whose names only contain 'zisk' are not ZisK crates" || fail "look-alike crate names" "rc=$RC $OUT"
finish guest_build_lock_check
