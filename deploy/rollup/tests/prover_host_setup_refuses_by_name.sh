#!/usr/bin/env bash
# deploy/rollup/tests/prover_host_setup_refuses_by_name.sh — the prover host setup script installs the ZisK GPU build the
# way the page says and refuses by name rather than half-installing. Nothing here installs anything: the script is run
# with stubs on PATH, and in --check mode, which changes nothing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
S="$ROLLUP_DIR/prover/setup-prover-host.sh"
[[ -f "$S" ]] || { fail "setup-prover-host.sh exists" "missing"; finish prover_host_setup_refuses_by_name; }
bash -n "$S" && pass "the script parses" || fail "the script parses" "bash -n failed"

# What it runs, read from the script: the GPU build, the keys, the PLONK key, an explicit prefix.
grep -qE 'ziskup.* -v "\$ZISK_VERSION" --gpu --provingkey -y --prefix "\$ZISK_HOME"|ZISKUP_BIN" -v "\$ZISK_VERSION" --gpu --provingkey -y --prefix "\$ZISK_HOME"' "$S" \
  && pass "ziskup is run with --gpu --provingkey -y --prefix \$ZISK_HOME" || fail "ziskup is run with --gpu --provingkey -y --prefix" "command line not found"
grep -qE 'ZISK_DIR="\$ZISK_HOME".*setup_snark' "$S" && pass "ziskup setup_snark installs into ZISK_HOME" || fail "ziskup setup_snark installs into ZISK_HOME" "not found"
# ziskup runs `cargo-zisk toolchain install`, which needs rustup: the script installs it (minimal) and puts
# ~/.cargo/bin on PATH, both before it runs ziskup.
rl="$(grep -n 'sh.rustup.rs' "$S" | head -1 | cut -d: -f1)"; pl="$(grep -n 'export PATH="\$HOME/.cargo/bin' "$S" | head -1 | cut -d: -f1)"
zl="$(grep -n '"\$ZISKUP_BIN" -v' "$S" | head -1 | cut -d: -f1)"
[[ -n "$rl" && -n "$zl" && "$rl" -lt "$zl" ]] && pass "rustup is installed before ziskup runs" || fail "rustup is installed before ziskup runs" "rustup line=${rl:-none} ziskup line=${zl:-none}"
grep -q -- '--profile minimal --default-toolchain none' "$S" && pass "rustup is installed minimal, with no default toolchain" || fail "rustup is installed minimal" "flags not found"
[[ -n "$pl" && -n "$zl" && "$pl" -lt "$zl" ]] && pass "~/.cargo/bin is on PATH before ziskup runs" || fail "~/.cargo/bin is on PATH before ziskup runs" "path line=${pl:-none} ziskup line=${zl:-none}"
grep -q '(cd "\$ZISK_HOME" && .*ZISKUP_BIN" -v' "$S" && grep -q '(cd "\$ZISK_HOME" && ZISK_DIR=.*setup_snark)' "$S" \
  && pass "ziskup and setup_snark run from inside ZISK_HOME" || fail "ziskup and setup_snark run from inside ZISK_HOME" "cd not found"
grep -q 'memlock unlimited' "$S" && pass "memlock unlimited is configured" || fail "memlock unlimited is configured" "not found"
grep -q 'nvidia-container-toolkit' "$S" && pass "nvidia-container-toolkit is installed" || fail "nvidia-container-toolkit is installed" "not found"
grep -q '\[gpu\]' "$S" && pass "the last check reads the [gpu] tag of cargo-zisk" || fail "the last check reads the [gpu] tag of cargo-zisk" "not found"
grep -q 'NotGpuBuild' "$S" && pass "a CPU-only cargo-zisk is refused by name (NotGpuBuild)" || fail "NotGpuBuild refusal" "not found"

# Run it for real, on a Linux x86_64 host and on anything else, with --check and no GPU tool on PATH.
mkdir -p "$WORK/emptybin"; for t in bash env uname dirname cat grep sed awk sort head tr id; do p="$(command -v $t)" && ln -sf "$p" "$WORK/emptybin/$t"; done
out="$(PATH="$WORK/emptybin" ZISK_HOME="$WORK/zisk" /bin/bash "$S" --check 2>&1)"; rc=$?
if [[ "$(uname -s)-$(uname -m)" == "Linux-x86_64" ]]; then want=NoGpuDriver; else want=UnsupportedPlatform; fi
if [[ $rc -ne 0 && "$out" == "$want:"* ]]; then pass "--check with no GPU driver refuses by name ($want)"; else fail "--check refuses by name ($want)" "rc=$rc out=$out"; fi
[[ ! -e "$WORK/zisk" ]] && pass "--check changed nothing" || fail "--check changed nothing" "$WORK/zisk was created"
# A plain run as a non-root user refuses before touching anything.
if [[ "$(id -u)" != 0 ]]; then
  out="$(ZISK_HOME="$WORK/zisk" bash "$S" 2>&1)"; rc=$?
  [[ $rc -ne 0 && "$out" == "NotRoot:"* ]] && pass "a run without root refuses by name (NotRoot)" || fail "a run without root refuses by name" "rc=$rc out=$out"
fi
bash "$S" --bogus >/dev/null 2>&1; [[ $? -ne 0 ]] && pass "an unknown argument is refused" || fail "an unknown argument is refused" "accepted"
finish prover_host_setup_refuses_by_name
