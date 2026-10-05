#!/usr/bin/env bash
# deploy/rollup/tests/prover_host_setup_refuses_by_name.sh — the prover host setup script installs the ZisK GPU build the
# way the page says and refuses by name rather than half-installing. Nothing here installs anything: the script is run
# with stubs on PATH, and in --check mode, which changes nothing.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
S="${SETUP_SCRIPT:-$ROLLUP_DIR/prover/setup-prover-host.sh}"
[[ -f "$S" ]] || { fail "setup-prover-host.sh exists" "missing"; finish prover_host_setup_refuses_by_name; }
bash -n "$S" && pass "the script parses" || fail "the script parses" "bash -n failed"
sha_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" 2>/dev/null | cut -d' ' -f1; else shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1; fi; }
sha_stdin() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

# What it runs, read from the script: the GPU build with no key (the keys are the checked archives), an explicit prefix.
grep -qE '"\$ZISKUP_BIN" -v "\$ZISK_VERSION" --gpu --nokey -y --prefix "\$ZISK_HOME"' "$S" \
  && pass "ziskup is run with --gpu --nokey -y --prefix \$ZISK_HOME" || fail "ziskup is run with --gpu --nokey -y --prefix" "command line not found"
grep -qE -- '--provingkey|setup_snark' <(grep -v '^ *#' "$S") && fail "ziskup downloads no key" "--provingkey or setup_snark is still run" || pass "ziskup downloads no key (no --provingkey, no setup_snark)"
# ziskup runs `cargo-zisk toolchain install`, which needs rustup: the script installs it (minimal) and puts
# ~/.cargo/bin on PATH, both before it runs ziskup.
rl="$(grep -n 'sh.rustup.rs' "$S" | head -1 | cut -d: -f1)"; pl="$(grep -n 'export PATH="\$HOME/.cargo/bin' "$S" | head -1 | cut -d: -f1)"
zl="$(grep -n '"\$ZISKUP_BIN" -v' "$S" | head -1 | cut -d: -f1)"
[[ -n "$rl" && -n "$zl" && "$rl" -lt "$zl" ]] && pass "rustup is installed before ziskup runs" || fail "rustup is installed before ziskup runs" "rustup line=${rl:-none} ziskup line=${zl:-none}"
grep -q -- '--profile minimal --default-toolchain none' "$S" && pass "rustup is installed minimal, with no default toolchain" || fail "rustup is installed minimal" "flags not found"
[[ -n "$pl" && -n "$zl" && "$pl" -lt "$zl" ]] && pass "~/.cargo/bin is on PATH before ziskup runs" || fail "~/.cargo/bin is on PATH before ziskup runs" "path line=${pl:-none} ziskup line=${zl:-none}"
grep -q '(cd "\$ZISK_HOME" && .*ZISKUP_BIN" -v' "$S" \
  && pass "ziskup runs from inside ZISK_HOME" || fail "ziskup runs from inside ZISK_HOME" "cd not found"
grep -q 'memlock unlimited' "$S" && pass "memlock unlimited is configured" || fail "memlock unlimited is configured" "not found"
grep -q 'nvidia-container-toolkit' "$S" && pass "nvidia-container-toolkit is installed" || fail "nvidia-container-toolkit is installed" "not found"
grep -q '\[gpu\]' "$S" && pass "the last check reads the [gpu] tag of cargo-zisk" || fail "the last check reads the [gpu] tag of cargo-zisk" "not found"
grep -q 'NotGpuBuild' "$S" && pass "a CPU-only cargo-zisk is refused by name (NotGpuBuild)" || fail "NotGpuBuild refusal" "not found"

# Run it for real, on a Linux x86_64 host and on anything else, with --check and no GPU tool on PATH.
mkdir -p "$WORK/emptybin"; for t in bash env uname dirname cat grep sed awk sort head tr id cut tail shasum sha256sum; do p="$(command -v $t)" && ln -sf "$p" "$WORK/emptybin/$t"; done
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

# --- the release the script installs, and the two checks that keep it that release ------------------------------------
PIN="$ROLLUP_DIR/guest-build/zisk/1.3.1-alpha.env"
pinned() { grep -E "^$1=" "$PIN" | head -n1 | cut -d= -f2-; }
grep -q 'ZISK_VERSION="${ZISK_VERSION:-1.3.1-alpha}"' "$S" && pass "the default release is 1.3.1-alpha" || fail "the default release is 1.3.1-alpha" "default not found"
grep -q '1\.2\.0' "$S" && fail "the script names no 1.2.0 release" "1.2.0 found" || pass "the script names no 1.2.0 release"
[[ "$(pinned ZISKUP_SHA256)" =~ ^[0-9a-f]{64}$ && "$(pinned ZISK_COMMIT)" =~ ^[0-9a-f]{40}$ ]] \
  && pass "the 1.3.1 pin file carries the ziskup sha256 and the commit" || fail "the 1.3.1 pin file carries the ziskup sha256 and the commit" "missing"
# A ziskup whose hash is not the pinned one is refused by name; --check-ziskup changes nothing and runs on any platform.
printf '#!/bin/sh\necho not the pinned installer\n' > "$WORK/ziskup-stub"; chmod +x "$WORK/ziskup-stub"
out="$(PATH="$WORK/emptybin" ZISKUP_BIN="$WORK/ziskup-stub" /bin/bash "$S" --check-ziskup 2>&1)"; rc=$?
if [[ $rc -ne 0 && "$out" == "ZiskupHashMismatch:"* && "$out" == *"$(pinned ZISKUP_SHA256)"* ]]; then pass "a ziskup that is not the pinned one is refused by name (ZiskupHashMismatch), naming the pin"; else fail "a ziskup that is not the pinned one is refused by name" "rc=$rc out=$out"; fi
stub_sha="$(shasum -a 256 "$WORK/ziskup-stub" 2>/dev/null | cut -d' ' -f1)"; [[ -n "$stub_sha" ]] || stub_sha="$(sha256sum "$WORK/ziskup-stub" | cut -d' ' -f1)"
printf 'ZISKUP_SHA256=%s\nZISK_COMMIT=%s\n' "$stub_sha" "$(pinned ZISK_COMMIT)" > "$WORK/stub.env"
out="$(PATH="$WORK/emptybin" PIN_FILE="$WORK/stub.env" ZISKUP_BIN="$WORK/ziskup-stub" /bin/bash "$S" --check-ziskup 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "a ziskup that has the pinned hash is accepted" || fail "a ziskup that has the pinned hash is accepted" "rc=$rc out=$out"
out="$(PATH="$WORK/emptybin" PIN_FILE="$WORK/none.env" ZISKUP_SHA256= ZISKUP_BIN="$WORK/ziskup-stub" /bin/bash "$S" --check-ziskup 2>&1)"; rc=$?
[[ $rc -ne 0 && "$out" == "ZiskPinMissing:"* ]] && pass "no pinned hash at all is refused by name (ZiskPinMissing)" || fail "no pinned hash at all is refused by name" "rc=$rc out=$out"
out="$(PATH="$WORK/emptybin" ZISKUP_BIN="$WORK/no-such-ziskup" /bin/bash "$S" --check-ziskup 2>&1)"; rc=$?
[[ $rc -ne 0 && "$out" == "ZiskupFailed:"* ]] && pass "a missing installer is refused by name (ZiskupFailed)" || fail "a missing installer is refused by name" "rc=$rc out=$out"

# cargo-zisk of another release, or of another commit, is WrongZiskVersion; a cargo-zisk that is not the pinned GPU binary
# is BinaryHashMismatch whatever version line it prints. Runs the real --check path on Linux x86_64 with a stub nvidia-smi
# and stub binaries under ZISK_HOME, whose hashes are pinned through a host pin file of the test's own.
if [[ "$(uname -s)-$(uname -m)" == "Linux-x86_64" ]]; then
  for t in find sha256sum mktemp; do p="$(command -v $t)" && ln -sf "$p" "$WORK/emptybin/$t"; done
  cat > "$WORK/emptybin/nvidia-smi" <<'SMI'
#!/bin/sh
case "$*" in
  *driver_version*) echo 550.54.15 ;;
  *memory.total*) echo 81920 ;;
  *) echo "Stub GPU" ;;
esac
SMI
  chmod +x "$WORK/emptybin/nvidia-smi"
  mkdir -p "$WORK/zh/bin"
  commit7="$(pinned ZISK_COMMIT | cut -c1-7)"
  printf '#!/bin/sh\necho "stub cargo-zisk-dev"\n' > "$WORK/zh/bin/cargo-zisk-dev"; chmod +x "$WORK/zh/bin/cargo-zisk-dev"
  say_version() { # $1 = the version line the stub prints; the pin file pins exactly this stub
    printf '#!/bin/sh\necho "%s"\n' "$1" > "$WORK/zh/bin/cargo-zisk"; chmod +x "$WORK/zh/bin/cargo-zisk"
    printf 'GPU_CARGO_ZISK_SHA256=%s\nGPU_CARGO_ZISK_DEV_SHA256=%s\n' "$(sha_of "$WORK/zh/bin/cargo-zisk")" "$(sha_of "$WORK/zh/bin/cargo-zisk-dev")" > "$WORK/hp.env"
  }
  run_check() { PATH="$WORK/emptybin" ZISK_HOME="$WORK/zh" HOST_PIN_FILE="$WORK/hp.env" /bin/bash "$S" --check 2>&1; }
  say_version "cargo-zisk 1.2.0-alpha [gpu] (fbbc69b stub-build-date)"; out="$(run_check)"; rc=$?
  [[ $rc -ne 0 && "$out" == *"WrongZiskVersion:"* && "$out" == *"1.2.0-alpha"* ]] && pass "a 1.2.0 cargo-zisk is refused by name (WrongZiskVersion)" || fail "a 1.2.0 cargo-zisk is refused by name" "rc=$rc out=$out"
  say_version "cargo-zisk 1.3.1-alpha [gpu] (abcdef0 stub-build-date)"; out="$(run_check)"; rc=$?
  [[ $rc -ne 0 && "$out" == *"WrongZiskVersion:"* && "$out" == *"commit $commit7"* ]] && pass "a 1.3.1 cargo-zisk of another commit is refused by name (WrongZiskVersion)" || fail "a 1.3.1 cargo-zisk of another commit is refused by name" "rc=$rc out=$out"
  say_version "cargo-zisk 1.3.1-alpha [gpu] ($commit7 stub-build-date)"; out="$(run_check)"; rc=$?
  [[ "$out" != *"WrongZiskVersion"* && "$out" == *"KeysShaMismatch:"* ]] && pass "the pinned release and commit pass the version check (the empty key directories are refused next)" || fail "the pinned release and commit pass the version check" "rc=$rc out=$out"
  # A substituted binary that prints the right version line: the version check cannot see it, the binary hash does.
  printf 'GPU_CARGO_ZISK_SHA256=%s\nGPU_CARGO_ZISK_DEV_SHA256=%s\n' "$(printf 'x' | sha_stdin)" "$(sha_of "$WORK/zh/bin/cargo-zisk-dev")" > "$WORK/hp.env"
  out="$(run_check)"; rc=$?
  [[ $rc -ne 0 && "$out" == *$'\n'"BinaryHashMismatch:"* && "$out" != *"WrongZiskVersion"* ]] && pass "a cargo-zisk that prints the right version line but is not the pinned binary is refused by name (BinaryHashMismatch)" || fail "a substituted cargo-zisk is refused by name" "rc=$rc out=$out"
else
  echo "SKIP: the cargo-zisk version checks run on Linux x86_64 only (this is $(uname -s) $(uname -m))"
fi

# --- the install path: what is checked, and when. Checked: ziskup before it runs; both key archives before either is
# unpacked; the two GPU binaries after ziskup installs them and before the script runs them. Not checked: the rest of the
# release's binary archive, and the cargo-zisk that ziskup itself runs (the stub below does not model them). ---------------
# The script is sourced (it runs nothing when sourced) and install_zisk is called with stubs: a stub ziskup that writes the
# two GPU binaries as stubs and logs that it ran, a stub cargo-zisk-dev that logs the constant-tree step, and two tiny key
# archives served from a file:// directory. Every pin is the sha256 of one of those stubs, given by environment.
# SETUP_SCRIPT runs the same cases against another copy of the script (the mutation checks below do).
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  build_case() { # $1 = case name; creates $WORK/c/$1 with the stubs and prints nothing
    local d="$WORK/c/$1"; rm -rf "$d"; mkdir -p "$d/src/pk/provingKey" "$d/src/sk/provingKeySnark" "$d/zh"
    echo key > "$d/src/pk/provingKey/k"; echo snark > "$d/src/sk/provingKeySnark/k"
    tar -czf "$d/src/zisk-provingkey-1.3.1-alpha.tar.gz" -C "$d/src/pk" provingKey
    tar -czf "$d/src/zisk-provingkey-plonk-1.3.1-alpha.tar.gz" -C "$d/src/sk" provingKeySnark
    cat > "$d/ziskup" <<ZSTUB
#!/bin/sh
BUCKET_URL="file://$d/src"
echo "ziskup \$*" >> "$d/log"
prefix=""; while [ \$# -gt 0 ]; do [ "\$1" = --prefix ] && prefix="\$2"; shift; done
# The real installer clears its prefix before it installs, so anything kept under ZISK_HOME is gone afterwards.
rm -rf "\$prefix"
mkdir -p "\$prefix/bin"
printf '#!/bin/sh\necho cargo-zisk\n' > "\$prefix/bin/cargo-zisk"
printf '#!/bin/sh\necho "cargo-zisk-dev \$*" >> "$d/log"\nif [ -e "$d/fail-once" ]; then rm -f "$d/fail-once"; exit 1; fi\n' > "\$prefix/bin/cargo-zisk-dev"
chmod +x "\$prefix/bin/cargo-zisk" "\$prefix/bin/cargo-zisk-dev"
ZSTUB
    chmod +x "$d/ziskup"
    # The bytes the stub ziskup writes, to pin: cargo-zisk, and cargo-zisk-dev with the log path in it (it fails once while
    # $d/fail-once exists).
    printf '#!/bin/sh\necho cargo-zisk\n' > "$d/exp-cz"
    printf '#!/bin/sh\necho "cargo-zisk-dev $*" >> "%s"\nif [ -e "%s" ]; then rm -f "%s"; exit 1; fi\n' "$d/log" "$d/fail-once" "$d/fail-once" > "$d/exp-dev-real"
  }
  run_case() { # $1 = case name, $2 = script, rest = KEY=VALUE overrides; prints the output, sets CASE_RC
    local c="$1" scr="$2" d="$WORK/c/$1"; shift 2
    env ZISK_HOME="$d/zh" DOWNLOAD_DIR="$d/dl" ZISKUP_BIN="$d/ziskup" MIN_FREE_GB=0 \
      KEYS_URL_BASE="file://$d/src" HOST_PIN_FILE="$d/host.env" ZISKUP_SHA256="$(sha_of "$d/ziskup")" \
      ARCHIVE_SHA256="$(sha_of "$d/src/zisk-provingkey-1.3.1-alpha.tar.gz")" \
      ARCHIVE_PLONK_SHA256="$(sha_of "$d/src/zisk-provingkey-plonk-1.3.1-alpha.tar.gz")" \
      GPU_CARGO_ZISK_SHA256="$(sha_of "$d/exp-cz")" GPU_CARGO_ZISK_DEV_SHA256="$(sha_of "$d/exp-dev-real")" \
      "$@" bash -c 'source "$1"; install_zisk' _ "$scr" > "$WORK/case.out" 2>&1
    CASE_RC=$?
  }
  rc_run() { run_case "$@"; out="$(cat "$WORK/case.out")"; }
  logged() { cat "$WORK/c/$1/log" 2>/dev/null; }
  # 1. everything matches: the install runs, in this order
  build_case ok;  rc_run ok "$S"
  if [[ $CASE_RC -eq 0 && -d "$WORK/c/ok/zh/provingKey" && -d "$WORK/c/ok/zh/provingKeySnark" ]]; then pass "an install whose every download matches its pin completes and unpacks both key sets"; else fail "a matching install completes" "rc=$CASE_RC out=$out"; fi
  [[ "$(logged ok | head -1)" == "ziskup -v 1.3.1-alpha --gpu --nokey -y --prefix $WORK/c/ok/zh" ]] && pass "ziskup is run with --gpu --nokey --prefix, and runs first" || fail "ziskup is run with --gpu --nokey --prefix" "$(logged ok)"
  logged ok | sed -n 2p | grep -q -E 'check-setup --proving-key .*/provingKey --proving-key-plonk .*/provingKeySnark --plonk --gpu$' && pass "the constant trees are generated after ziskup, by the checked cargo-zisk-dev, for both key sets with aggregation and PLONK" || fail "the constant trees are generated for both key sets" "$(logged ok)"
  # -a is --no-aggregation in check-setup: with it, a GPU host gets no compressor or recursive *.const_gpu files and every
  # proof fails at the first aggregation step.
  ! logged ok | grep -q -E 'check-setup.* (-a|--no-aggregation)( |$)' && pass "check-setup keeps aggregation on (no -a)" || fail "check-setup keeps aggregation on" "$(logged ok)"
  [[ ! -e "$WORK/c/ok/dl/zisk-provingkey-1.3.1-alpha.tar.gz" && ! -e "$WORK/c/ok/dl/zisk-provingkey-plonk-1.3.1-alpha.tar.gz" ]] && pass "the downloaded archives are deleted after they are unpacked" || fail "the archives are deleted" "$(ls "$WORK/c/ok/dl" 2>&1)"

  # 1b. The key archives are kept outside ZISK_HOME, which ziskup clears: with the default DOWNLOAD_DIR the install still
  # finds and unpacks them, and a DOWNLOAD_DIR inside ZISK_HOME is refused before anything runs or is downloaded.
  build_case dldef; rc_run dldef "$S" DOWNLOAD_DIR=
  [[ $CASE_RC -eq 0 && -d "$WORK/c/dldef/zh/provingKey" && -d "$WORK/c/dldef/zh/provingKeySnark" ]] \
    && pass "with the default DOWNLOAD_DIR the archives survive ziskup clearing ZISK_HOME and both key sets are unpacked" || fail "the default DOWNLOAD_DIR survives ziskup" "rc=$CASE_RC out=$out"
  build_case dlin; rc_run dlin "$S" DOWNLOAD_DIR="$WORK/c/dlin/zh/dl"
  [[ $CASE_RC -ne 0 && "$out" == *"DownloadDirInsideZiskHome:"* && -z "$(logged dlin)" && ! -e "$WORK/c/dlin/zh/dl/zisk-provingkey-1.3.1-alpha.tar.gz" ]] \
    && pass "a DOWNLOAD_DIR inside ZISK_HOME is refused by name (DownloadDirInsideZiskHome) before ziskup runs or anything is downloaded" || fail "a DOWNLOAD_DIR inside ZISK_HOME is refused" "rc=$CASE_RC out=$out"

  # 1c. check-setup is a step of its own. If it fails or is killed, the keys are already unpacked and match the pins, so a
  # re-run must run it again; a done marker, written only after it succeeds and kept outside the hashed key directories,
  # is what says it ran. A marker from another release does not count, and the last check refuses without one.
  stub_manifest() { # $1 = case name; pins the stub keys of that case in $WORK/c/$1/keys.sha256
    local d="$WORK/c/$1"; rm -rf "$d/pinsrc"; mkdir -p "$d/pinsrc"; cp -R "$d/src/pk/provingKey" "$d/src/sk/provingKeySnark" "$d/pinsrc/"
    ZISK_HOME="$d/pinsrc" MANIFEST="$d/keys.sha256" bash "$(dirname "$S")/check-keys.sh" --write >/dev/null 2>&1
  }
  count_setup() { logged "$1" | grep -c check-setup; }
  build_case redo; stub_manifest redo; touch "$WORK/c/redo/fail-once"
  rc_run redo "$S" MANIFEST="$WORK/c/redo/keys.sha256"
  [[ $CASE_RC -ne 0 && "$out" == *"ZiskupFailed:"* && "$(count_setup redo)" == 1 && -d "$WORK/c/redo/zh/provingKey" ]] \
    && pass "a check-setup that fails stops the install by name (ZiskupFailed) with the keys already unpacked" || fail "a failing check-setup stops the install" "rc=$CASE_RC out=$out log=$(logged redo)"
  ls "$WORK/c/redo/zh"/.check-setup-done* >/dev/null 2>&1 && fail "no done marker after a failed check-setup" "$(ls -a "$WORK/c/redo/zh")" || pass "no done marker is written after a failed check-setup"
  rc_run redo "$S" MANIFEST="$WORK/c/redo/keys.sha256"
  [[ $CASE_RC -eq 0 && "$out" == *"already match"* && "$(count_setup redo)" == 2 ]] \
    && pass "the re-run sees the keys match, runs check-setup again, and completes" || fail "the re-run runs check-setup again" "rc=$CASE_RC out=$out log=$(logged redo)"
  ls "$WORK/c/redo/zh"/.check-setup-done* >/dev/null 2>&1 && pass "the done marker is written once check-setup succeeds" || fail "the done marker is written" "$(ls -a "$WORK/c/redo/zh")"
  rc_run redo "$S" MANIFEST="$WORK/c/redo/keys.sha256"
  [[ $CASE_RC -eq 0 && "$(count_setup redo)" == 2 ]] && pass "with the marker present a further run does not run check-setup again" || fail "a run with the marker skips check-setup" "rc=$CASE_RC log=$(logged redo)"
  mv "$WORK/c/redo/zh"/.check-setup-done* "$WORK/c/redo/zh/.check-setup-done-0.0.1-other"
  rc_run redo "$S" MANIFEST="$WORK/c/redo/keys.sha256"
  [[ $CASE_RC -eq 0 && "$(count_setup redo)" == 3 ]] && pass "a marker left by another release does not count: check-setup runs again" || fail "another release's marker does not count" "rc=$CASE_RC log=$(logged redo)"
  out="$(env ZISK_HOME="$WORK/c/redo/zh" bash -c 'source "$1"; require_setup_done' _ "$S" 2>&1)"; rc=$?
  [[ $rc -eq 0 ]] && pass "the last check accepts a host whose check-setup has finished" || fail "the last check accepts a finished host" "rc=$rc out=$out"
  rm -f "$WORK/c/redo/zh"/.check-setup-done*
  out="$(env ZISK_HOME="$WORK/c/redo/zh" bash -c 'source "$1"; require_setup_done' _ "$S" 2>&1)"; rc=$?
  [[ $rc -ne 0 && "$out" == "ConstantFilesMissing:"* && "$out" == *"check-setup"* ]] && pass "the last check refuses a host whose check-setup has not finished (ConstantFilesMissing), naming the command" || fail "the last check refuses without the marker" "rc=$rc out=$out"

  # 2. ziskup that is not the pinned one never runs, and nothing is downloaded: the order inside install_zisk itself.
  zup_bad() { # a ziskup that does not have the pinned hash (the pin is the hash of the stub before it was changed)
    build_case "$1"; local good; good="$(sha_of "$WORK/c/$1/ziskup")"
    printf '\n# changed\n' >> "$WORK/c/$1/ziskup"; rc_run "$1" "$2" ZISKUP_SHA256="$good"
  }
  zup_bad zup "$S"; rc=$CASE_RC
  [[ $rc -ne 0 && "$out" == *"ZiskupHashMismatch:"* && -z "$(logged zup)" && ! -e "$WORK/c/zup/dl/zisk-provingkey-1.3.1-alpha.tar.gz" ]] \
    && pass "install_zisk refuses a ziskup that is not the pinned one (ZiskupHashMismatch) before it runs and before any download" || fail "install_zisk refuses an unpinned ziskup before it runs" "rc=$rc out=$out log=$(logged zup)"
  # A ziskup that is not on the host is downloaded to a file of its own, and a bad download never reaches ZISKUP_BIN.
  build_case dlz;  printf 'not the installer\n' > "$WORK/c/dlz/fetched"; rm -f "$WORK/c/dlz/ziskup"
  rc_run dlz "$S" ZISKUP_URL="file://$WORK/c/dlz/fetched" ZISKUP_SHA256="$(sha_of "$WORK/c/ok/ziskup")"
  [[ $CASE_RC -ne 0 && "$out" == *"ZiskupHashMismatch:"* && ! -e "$WORK/c/dlz/ziskup" ]] && pass "a downloaded ziskup that is not the pinned one is refused and never placed at ZISKUP_BIN" || fail "a bad downloaded ziskup is refused" "rc=$CASE_RC out=$out"

  # 2b. Where the key archives come from: the BUCKET_URL line of the checked ziskup, unless KEYS_URL_BASE is set. The stub
  # ziskup names its own source folder; the script itself names no host.
  build_case urlz; rc_run urlz "$S" KEYS_URL_BASE=
  [[ $CASE_RC -eq 0 && -d "$WORK/c/urlz/zh/provingKey" && -d "$WORK/c/urlz/zh/provingKeySnark" ]] \
    && pass "with no KEYS_URL_BASE the key archives are downloaded from the BUCKET_URL line of the checked ziskup" || fail "the key archive location comes from ziskup" "rc=$CASE_RC out=$out"
  build_case urlo; sed -i.bak "s#^BUCKET_URL=.*#BUCKET_URL=\"file://$WORK/c/urlo/nowhere\"#" "$WORK/c/urlo/ziskup"; rm -f "$WORK/c/urlo/ziskup.bak"
  rc_run urlo "$S" KEYS_URL_BASE="file://$WORK/c/urlo/src"
  [[ $CASE_RC -eq 0 && -d "$WORK/c/urlo/zh/provingKey" ]] \
    && pass "KEYS_URL_BASE wins over the location ziskup names" || fail "KEYS_URL_BASE wins over ziskup's location" "rc=$CASE_RC out=$out"
  build_case urln; sed -i.bak "s#^BUCKET_URL=.*#BUCKET_URL=\"file://$WORK/c/urln/nowhere\"#" "$WORK/c/urln/ziskup"; rm -f "$WORK/c/urln/ziskup.bak"
  rc_run urln "$S" KEYS_URL_BASE=
  [[ $CASE_RC -ne 0 && "$out" == *"ZiskupFailed:"* && "$out" == *"urln/nowhere"* && ! -e "$WORK/c/urln/zh/provingKey" ]] \
    && pass "the location in ziskup is the one that is tried (a wrong one fails the download, nothing is unpacked)" || fail "the location in ziskup is the one tried" "rc=$CASE_RC out=$out"
  build_case urlm; sed -i.bak '/^BUCKET_URL=/d' "$WORK/c/urlm/ziskup"; rm -f "$WORK/c/urlm/ziskup.bak"
  rc_run urlm "$S" KEYS_URL_BASE=
  [[ $CASE_RC -ne 0 && "$out" == *"KeysUrlMissing:"* && "$out" == *"KEYS_URL_BASE"* && ! -e "$WORK/c/urlm/zh/provingKey" && -z "$(logged urlm)" ]] \
    && pass "with neither KEYS_URL_BASE nor a BUCKET_URL line in ziskup the script refuses by name (KeysUrlMissing), before anything is downloaded or run" || fail "no download location is refused by name" "rc=$CASE_RC out=$out log=$(logged urlm)"
  build_case urlx; sed -i.bak 's#^BUCKET_URL=.*#BUCKET_URL="file://$(touch '"$WORK"'/urlx-ran)"#' "$WORK/c/urlx/ziskup"; rm -f "$WORK/c/urlx/ziskup.bak"
  rc_run urlx "$S" KEYS_URL_BASE=
  [[ $CASE_RC -ne 0 && "$out" == *"KeysUrlMissing:"* && ! -e "$WORK/urlx-ran" ]] \
    && pass "a BUCKET_URL line that is not a plain URL is not used, and nothing in it is run" || fail "a BUCKET_URL that is not a plain URL is refused" "rc=$CASE_RC out=$out"

  # 3. a key archive that is not the pinned one is refused by name before either archive is unpacked and before ziskup runs.
  for k in ARCHIVE_SHA256 ARCHIVE_PLONK_SHA256; do
    build_case "arch-$k";  rc_run "arch-$k" "$S" "$k=$(printf 'other' | sha_stdin)"
    [[ $CASE_RC -ne 0 && "$out" == *"KeyArchiveHashMismatch:"* && ! -e "$WORK/c/arch-$k/zh/provingKey" && ! -e "$WORK/c/arch-$k/zh/provingKeySnark" && -z "$(logged "arch-$k")" ]] \
      && pass "a key archive that is not the pinned one is refused by name (KeyArchiveHashMismatch) before either archive is unpacked and before ziskup runs ($k)" || fail "a bad key archive is refused ($k)" "rc=$CASE_RC out=$out log=$(logged "arch-$k")"
    ls "$WORK/c/arch-$k/dl/"*.tar.gz >/dev/null 2>&1 && [[ "$k" == ARCHIVE_SHA256 ]] && fail "a bad archive is not kept under its final name" "$(ls "$WORK/c/arch-$k/dl")"
  done

  # 4. a GPU binary that is not the pinned one is refused by name after ziskup has installed it and before this script runs
  # it (the constant-tree step) or unpacks the keys.
  for k in GPU_CARGO_ZISK_SHA256 GPU_CARGO_ZISK_DEV_SHA256; do
    build_case "bin-$k";  rc_run "bin-$k" "$S" "$k=$(printf 'other' | sha_stdin)"
    [[ $CASE_RC -ne 0 && "$out" == *"BinaryHashMismatch:"* && ! -e "$WORK/c/bin-$k/zh/provingKey" ]] && ! logged "bin-$k" | grep -q check-setup \
      && pass "a GPU binary that is not the pinned one is refused by name (BinaryHashMismatch) before the constant trees are generated or the keys are unpacked ($k)" || fail "a bad GPU binary is refused ($k)" "rc=$CASE_RC out=$out log=$(logged "bin-$k")"
  done

  # 5. no pin at all is ZiskPinMissing, for each of the four.
  for k in ARCHIVE_SHA256 ARCHIVE_PLONK_SHA256 GPU_CARGO_ZISK_SHA256 GPU_CARGO_ZISK_DEV_SHA256; do
    build_case "pin-$k";  rc_run "pin-$k" "$S" "$k="
    [[ $CASE_RC -ne 0 && "$out" == *"ZiskPinMissing:"* && "$out" == *"$k"* ]] && pass "a missing $k pin is refused by name (ZiskPinMissing)" || fail "a missing $k pin is refused" "rc=$CASE_RC out=$out"
  done

  # 6. the same checks, one at a time, change nothing: --check-archives and --check-binaries.
  d="$WORK/c/ok"; mkdir -p "$d/dl2"; cp "$d/src/"*.tar.gz "$d/dl2/"
  env DOWNLOAD_DIR="$d/dl2" HOST_PIN_FILE="$d/host.env" ARCHIVE_SHA256="$(sha_of "$d/src/zisk-provingkey-1.3.1-alpha.tar.gz")" ARCHIVE_PLONK_SHA256="$(sha_of "$d/src/zisk-provingkey-plonk-1.3.1-alpha.tar.gz")" bash "$S" --check-archives >/dev/null 2>&1 \
    && pass "--check-archives accepts archives with the pinned hashes" || fail "--check-archives accepts pinned archives" "refused"
  echo x >> "$d/dl2/zisk-provingkey-plonk-1.3.1-alpha.tar.gz"
  out="$(env DOWNLOAD_DIR="$d/dl2" HOST_PIN_FILE="$d/host.env" ARCHIVE_SHA256="$(sha_of "$d/src/zisk-provingkey-1.3.1-alpha.tar.gz")" ARCHIVE_PLONK_SHA256="$(sha_of "$d/src/zisk-provingkey-plonk-1.3.1-alpha.tar.gz")" bash "$S" --check-archives 2>&1)"; rc=$?
  [[ $rc -ne 0 && "$out" == *"KeyArchiveHashMismatch:"* ]] && pass "--check-archives refuses a changed archive by name (KeyArchiveHashMismatch)" || fail "--check-archives refuses a changed archive" "rc=$rc out=$out"
  rm "$d/dl2/zisk-provingkey-plonk-1.3.1-alpha.tar.gz"
  out="$(env DOWNLOAD_DIR="$d/dl2" HOST_PIN_FILE="$d/host.env" ARCHIVE_SHA256="$(sha_of "$d/src/zisk-provingkey-1.3.1-alpha.tar.gz")" ARCHIVE_PLONK_SHA256="$(sha_of "$d/src/zisk-provingkey-plonk-1.3.1-alpha.tar.gz")" bash "$S" --check-archives 2>&1)"; rc=$?
  [[ $rc -ne 0 && "$out" == *"KeyArchiveMissing:"* ]] && pass "--check-archives names a missing archive (KeyArchiveMissing)" || fail "--check-archives names a missing archive" "rc=$rc out=$out"
  env ZISK_HOME="$d/zh" HOST_PIN_FILE="$d/host.env" GPU_CARGO_ZISK_SHA256="$(sha_of "$d/exp-cz")" GPU_CARGO_ZISK_DEV_SHA256="$(sha_of "$d/exp-dev-real")" bash "$S" --check-binaries >/dev/null 2>&1 \
    && pass "--check-binaries accepts the pinned binaries" || fail "--check-binaries accepts the pinned binaries" "refused"
  out="$(env ZISK_HOME="$d/zh" HOST_PIN_FILE="$d/host.env" GPU_CARGO_ZISK_SHA256="$(printf 'z' | sha_stdin)" GPU_CARGO_ZISK_DEV_SHA256="$(sha_of "$d/exp-dev-real")" bash "$S" --check-binaries 2>&1)"; rc=$?
  [[ $rc -ne 0 && "$out" == *"BinaryHashMismatch:"* ]] && pass "--check-binaries refuses another binary by name (BinaryHashMismatch)" || fail "--check-binaries refuses another binary" "rc=$rc out=$out"

  # 7. The checks are what stops those cases: with each real check deleted from a copy of the script, the matching case
  # above is no longer refused. (Deleting the check must fail this test.)
  # A mutant sits in a folder of its own with the two files the script reads from its own folder.
  mutant() { # $1 = name, $2 = perl substitution; makes $WORK/mut/$1/setup.sh from the script
    mkdir -p "$WORK/mut/$1"; cp "$(dirname "$S")/check-keys.sh" "$(dirname "$S")/keys.sha256" "$WORK/mut/$1/"
    perl -0pe "$2" "$S" > "$WORK/mut/$1/setup.sh"; cmp -s "$S" "$WORK/mut/$1/setup.sh" && fail "mutant $1 changes the script" "substitution matched nothing"
  }
  # ziskup's own check inside install_zisk (two-space indent; the --check-ziskup branch is indented deeper).
  mutant ziskup 's/\n  verify_ziskup "\$ZISKUP_BIN"\n/\n/'
  zup_bad zupm "$WORK/mut/ziskup/setup.sh"; rc=$CASE_RC
  [[ -n "$(logged zupm)" && "$out" != *"ZiskupHashMismatch"* ]] && pass "mutation: without install_zisk's own ziskup check, an unpinned ziskup runs (so the test above catches its removal)" || fail "mutation: ziskup check removed is caught" "rc=$rc log=$(logged zupm)"
  mutant archives 's/if \[\[ "\$got" != "\$3" \]\]; then/if false; then/'
  build_case archm;  rc_run archm "$WORK/mut/archives/setup.sh" ARCHIVE_SHA256="$(printf 'other' | sha_stdin)"
  [[ "$out" != *"KeyArchiveHashMismatch"* && -d "$WORK/c/archm/zh/provingKey" ]] && pass "mutation: without the archive check, a changed key archive is unpacked (so the test above catches its removal)" || fail "mutation: archive check removed is caught" "rc=$CASE_RC out=$out"
  mutant binaries 's/\n  verify_binaries\n  install_keys\n/\n  install_keys\n/'
  build_case binm;  rc_run binm "$WORK/mut/binaries/setup.sh" GPU_CARGO_ZISK_SHA256="$(printf 'other' | sha_stdin)"
  [[ "$out" != *"BinaryHashMismatch"* ]] && logged binm | grep -q check-setup && pass "mutation: without the binary check, a changed cargo-zisk is run (so the test above catches its removal)" || fail "mutation: binary check removed is caught" "rc=$CASE_RC out=$out log=$(logged binm)"
else
  echo "SKIP: the install-path cases need GNU tar (this host has $(tar --version 2>/dev/null | head -1))"
fi

finish prover_host_setup_refuses_by_name
