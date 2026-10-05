#!/usr/bin/env bash
# deploy/rollup/tests/guest_build_command.sh — `rollup guest-build` with a stub docker: what it refuses by name before it
# runs anything, what it passes to the image (the genesis and the chain id from chain-id.env, read-only, the keys only when
# ZISK_HOME holds them), and what it reports. The image's own script is tested in guest_build_image_script.sh.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
GB_DIR="$ROLLUP_DIR/guest-build"

setup_fixture
"$ROLLUP" init >/dev/null 2>&1 || { fail "test setup: rollup init" "failed"; finish guest_build_command; }
CHAIN_ID="$(sed -n 's/^CHAIN_ID=//p' "$WORK/out/chain-id.env")"
GOOD_GENESIS="$WORK/good.genesis.json"; cp "$WORK/out/genesis.json" "$GOOD_GENESIS"

# A stub docker for this test only. `docker build` records its arguments. `docker run` records them and the mounts, then acts like
# the image: it writes <sha256 of its ELF>.elf and a vkey.json into the /out mount and prints what the image prints. The file
# $WORK/stub_fail makes the run fail; $WORK/stub_vk_none makes it answer without a programVK even when keys are mounted.
cat > "$WORK/initbin/docker" <<STUB
#!/usr/bin/env bash
W="$WORK"
case "\$1" in
  build) shift; echo "\$*" >> "\$W/build_calls"; exit 0 ;;
  run)
    shift; echo "\$*" >> "\$W/run_calls"
    out=""; keys=0; expect=""; args=("\$@")
    for ((i=0;i<\${#args[@]};i++)); do
      case "\${args[i]}" in
        --mount) m="\${args[i+1]}"; echo "\$m" >> "\$W/mounts"; case "\$m" in *target=/out*) out="\$(sed -n 's/.*source=\\([^,]*\\),target=\\/out.*/\\1/p' <<<"\$m")" ;; esac ;;
        --expect-chain-id) expect="\${args[i+1]}" ;;
        --proving-key-dir) keys=1 ;;
      esac
    done
    [ ! -f "\$W/stub_fail" ] || { echo "GuestBuildFailed: stub build failure" >&2; exit 1; }
    sha="\$(printf 'stub elf for %s' "\$expect" | python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"
    printf 'stub elf for %s' "\$expect" > "\$out/\$sha.elf"; echo '{}' > "\$out/vkey.json"
    [ -z "\${STUB_BAD_SHA:-}" ] || sha="\$STUB_BAD_SHA"
    echo "elf_sha256=\$sha"; echo "chain_id=\$expect"
    if [ "\$keys" = 1 ] && [ ! -f "\$W/stub_vk_none" ]; then echo "program_vk=0x\$(printf 'ab%.0s' \$(seq 32))"; else echo "program_vk=none"; echo "ProgramVkNotComputed: stub says so" >&2; fi
    exit 0 ;;
esac
echo "docker \$*" >> "\$W/docker_calls"; exit 0
STUB
chmod +x "$WORK/initbin/docker"
reset_calls() { : > "$WORK/build_calls"; : > "$WORK/run_calls"; : > "$WORK/mounts"; : > "$WORK/docker_calls"; rm -f "$WORK/stub_fail" "$WORK/stub_vk_none"; rm -rf "$WORK/out/guest"; cp "$GOOD_GENESIS" "$WORK/out/genesis.json"; }
nothing_ran() { [[ ! -s "$WORK/build_calls" && ! -s "$WORK/run_calls" ]]; }
refuses() { # $1=name, rest = ./rollup arguments; nothing may have been built or run
  local name="$1" out; shift
  if out="$("$ROLLUP" guest-build "$@" 2>&1)"; then fail "refuses by name ($name): guest-build $*" "exited 0"
  elif ! grep -q "^$name" <<<"$out"; then fail "refuses by name ($name): guest-build $*" "$out"
  elif ! nothing_ran; then fail "refuses by name ($name): guest-build $*" "docker ran: $(cat "$WORK/build_calls" "$WORK/run_calls")"
  else pass "refuses by name ($name): guest-build $* — and builds and runs nothing"; fi
}

# ---- the refusals ----------------------------------------------------------------------------------------------------
reset_calls
refuses UnknownArgument --colour blue
# A genesis whose chainId is not the chain's id: the chain id comes from chain-id.env.
jq --argjson id "$((CHAIN_ID + 1))" '.config.chainId = $id' "$GOOD_GENESIS" > "$WORK/out/genesis.json"
refuses ChainIdMismatch
out="$("$ROLLUP" guest-build 2>&1)"; grep -q "$((CHAIN_ID + 1))" <<<"$out" && grep -q "$CHAIN_ID" <<<"$out" && pass "ChainIdMismatch names both ids" || fail "ChainIdMismatch names both ids" "$out"
# More than one non-zero balance.
jq '.alloc["0x1111111111111111111111111111111111111111"] = {"balance":"0x1"} | .alloc["0x2222222222222222222222222222222222222222"] = {"balance":"2"}' "$GOOD_GENESIS" > "$WORK/out/genesis.json"
refuses GenesisFundedAccountLimit
# One non-zero balance is allowed (the guest build then prints it); zero balances are not counted.
jq '.alloc["0x1111111111111111111111111111111111111111"] = {"balance":"0x1"} | .alloc["0x2222222222222222222222222222222222222222"] = {"balance":"0x0"}' "$GOOD_GENESIS" > "$WORK/out/genesis.json"
: > "$WORK/build_calls"; : > "$WORK/run_calls"
if "$ROLLUP" guest-build >/dev/null 2>&1 && [[ -s "$WORK/run_calls" ]]; then pass "one funded account and one zero balance is accepted and run"; else fail "one funded account is accepted" "refused or not run"; fi
reset_calls
echo '{"alloc":' > "$WORK/out/genesis.json"
refuses GenesisInvalid
reset_calls
rm "$WORK/out/chain-id.env"
refuses NotInitialised
printf 'AUTHORITY=x\nNONCE=0\n' > "$WORK/out/chain-id.env"
refuses ChainIdMissing
printf 'AUTHORITY=x\nNONCE=0\nCHAIN_ID=%s\n' "$CHAIN_ID" > "$WORK/out/chain-id.env"
rm -rf "$WORK/emptyout"; ROLLUP_OUT="$WORK/emptyout" refuses NotInitialised
# ZISK_HOME set but without the keys.
mkdir -p "$WORK/zh-empty"
printf 'ZISK_HOME=%s\n' "$WORK/zh-empty" >> "$WORK/rollup.env"
refuses ProvingKeyDirMissing
out="$("$ROLLUP" guest-build 2>&1)"; grep -q 'ZisK 1.3.1-alpha proving keys' <<<"$out" && pass "ProvingKeyDirMissing names the release whose keys are needed" || fail "ProvingKeyDirMissing names the release" "$out"
sed -i.bak '/^ZISK_HOME=/d' "$WORK/rollup.env"; rm -f "$WORK/rollup.env.bak"

# ---- without keys: the ELF and a vkey.json without programVK, said by name ---------------------------------------------
reset_calls
out="$("$ROLLUP" guest-build 2>"$WORK/stderr")"; rc=$?
[[ $rc -eq 0 ]] && pass "without ZISK_HOME the build succeeds" || fail "without ZISK_HOME the build succeeds" "exit $rc: $(cat "$WORK/stderr")"
ELF_SHA="$(sed -n 's/^elf sha256 *//p' <<<"$out")"
[[ -f "$WORK/out/guest/$ELF_SHA.elf" && -f "$WORK/out/guest/vkey.json" ]] && pass "writes rendered/guest/<sha256>.elf and rendered/guest/vkey.json" || fail "writes the ELF and vkey.json" "$(ls "$WORK/out/guest" 2>&1)"
grep -q "^program VK *none" <<<"$out" && pass "reports the programVK as none" || fail "reports the programVK as none" "$out"
grep -q '^ProgramVkNotComputed' "$WORK/stderr" && pass "names ProgramVkNotComputed on stderr" || fail "names ProgramVkNotComputed" "$(cat "$WORK/stderr")"
grep -q 'ELF_DIR=.*guest' <<<"$out" && grep -q 'VKEY_JSON=.*vkey.json' <<<"$out" && pass "tells the operator what ELF_DIR and VKEY_JSON to set" || fail "ELF_DIR and VKEY_JSON hint" "$out"
grep -q 'proving-key-dir' "$WORK/run_calls" && fail "no keys are passed without ZISK_HOME" "$(cat "$WORK/run_calls")" || pass "no keys are passed without ZISK_HOME"
grep -q 'provingKey' "$WORK/mounts" && fail "no key mount without ZISK_HOME" "$(cat "$WORK/mounts")" || pass "no key mount without ZISK_HOME"

# ---- what the image gets ------------------------------------------------------------------------------------------------
grep -q -- "--expect-chain-id $CHAIN_ID" "$WORK/run_calls" && pass "the chain id from chain-id.env is passed as --expect-chain-id" || fail "--expect-chain-id" "$(cat "$WORK/run_calls")"
grep -q "source=$WORK/out/genesis.json,target=/in/genesis.json,readonly" "$WORK/mounts" && pass "rendered/genesis.json is mounted read-only" || fail "genesis mount" "$(cat "$WORK/mounts")"
grep -q "source=$WORK/out/guest,target=/out$" "$WORK/mounts" && pass "rendered/guest is the one writable mount" || fail "output mount" "$(cat "$WORK/mounts")"
grep -vc 'readonly$' "$WORK/mounts" | grep -qx 1 && pass "no other mount is writable" || fail "no other mount is writable" "$(cat "$WORK/mounts")"
grep -q -- '--platform linux/amd64' "$WORK/run_calls" && grep -q -- '--platform linux/amd64' "$WORK/build_calls" && pass "build and run both use linux/amd64, so any host gives the same ELF" || fail "platform" "$(cat "$WORK/build_calls" "$WORK/run_calls")"
grep -q -- '--rm' "$WORK/run_calls" && pass "the container is removed after the run" || fail "--rm" "$(cat "$WORK/run_calls")"

# ---- the image: tags and pins --------------------------------------------------------------------------------------------
grep -q -- '-t rome-zk-guest-build:1.3.1-alpha-v0.3.0-v0.3.0 ' "$WORK/build_calls" && grep -q 'ROME_ZK_EVM_TAG=v0.3.0' "$WORK/build_calls" && grep -q 'ROME_ZK_GUEST_TAG=v0.3.0' "$WORK/build_calls" && pass "default tags: node v0.3.0, guest v0.3.0, in an image named for the release" || fail "default tags" "$(cat "$WORK/build_calls")"
grep -q -- '--build-arg ZISK_RELEASE=1.3.1-alpha ' "$WORK/build_calls" && grep -q -- '--build-arg ZISK_TOOLCHAIN_TAG=zisk-4.0.0 ' "$WORK/build_calls" && pass "the default release is 1.3.1-alpha, and its toolchain tag comes from its pin file" || fail "default release build arguments" "$(cat "$WORK/build_calls")"
grep -q 'guest-build$' "$WORK/build_calls" && pass "the build context is deploy/rollup/guest-build" || fail "build context" "$(cat "$WORK/build_calls")"
# The Dockerfile's own defaults are the same tags, so a by-hand build gives the same image, and its labels are what was used.
grep -q '^ARG ROME_ZK_EVM_TAG=v0.3.0$' "$GB_DIR/Dockerfile" && grep -q '^ARG ROME_ZK_GUEST_TAG=v0.3.0$' "$GB_DIR/Dockerfile" && pass "the Dockerfile defaults are the same tags" || fail "Dockerfile defaults" "$(grep '^ARG' "$GB_DIR/Dockerfile")"
grep -q '^FROM ubuntu@sha256:[0-9a-f]\{64\}$' "$GB_DIR/Dockerfile" && pass "the base image is pinned by digest" || fail "base image pinned by digest" "$(grep '^FROM' "$GB_DIR/Dockerfile")"
# ---- the pins: one file per ZisK release, read by the Dockerfile and by the image's script ---------------------------------
DF="$GB_DIR/Dockerfile"; PINS="$GB_DIR/zisk"
grep -q '^ARG ZISK_RELEASE=1\.3\.1-alpha$' "$DF" && pass "the Dockerfile takes the release as a build argument, ZISK_RELEASE (1.3.1-alpha until a build names another)" || fail "ZISK_RELEASE build argument" "$(grep -n 'ZISK_RELEASE' "$DF" | head -3)"
# The default release is one the command and the Dockerfile agree on, and the Dockerfile's toolchain tag is that release's own pin.
dfl_rel="$(sed -n 's/^GUEST_BUILD_ZISK_RELEASE_DEFAULT=//p' "$ROLLUP")"
[[ "$dfl_rel" == 1.3.1-alpha && "$dfl_rel" == "$(sed -n 's/^ARG ZISK_RELEASE=//p' "$DF")" && "$(sed -n 's/^ARG ZISK_TOOLCHAIN_TAG=//p' "$DF")" == "$(sed -n 's/^ZISK_TOOLCHAIN_TAG=//p' "$PINS/$dfl_rel.env")" ]] && pass "the command and the Dockerfile default to ZisK 1.3.1-alpha, and the Dockerfile's toolchain tag is that release's pin" || fail "default release and toolchain tag agree" "command '$dfl_rel' Dockerfile '$(grep -E '^ARG ZISK_(RELEASE|TOOLCHAIN_TAG)=' "$DF" | tr '\n' ' ')'"
for rel in 1.2.0-alpha 1.3.1-alpha; do
  f="$PINS/$rel.env"
  if [[ ! -f "$f" ]]; then fail "pin file for $rel" "$f is missing"; continue; fi
  pin() { sed -n "s/^$1=//p" "$f" | head -n 1; }
  bad="$(grep -vE '^(#.*|[A-Z0-9_]+=[^[:space:]$`"'"'"']*)?$' "$f")"
  [[ -z "$bad" ]] && pass "$rel.env is plain KEY=VALUE lines and comments, nothing a shell would expand" || fail "$rel.env is plain KEY=VALUE" "$bad"
  missing=""; for k in ZISK_VERSION ZISK_SCHEME ZISK_COMMIT ZISKUP_URL ZISKUP_SHA256 ZISK_TOOLCHAIN_TAG ZISK_TOOLCHAIN_URL ZISK_TOOLCHAIN_SHA256 CARGO_ZISK_URL CARGO_ZISK_SHA256 ROOT_C_VADCOP_FINAL; do
    [[ -n "$(pin $k)" ]] || missing="$missing $k"; done
  [[ -z "$missing" ]] && pass "$rel.env names every pin:$(printf ' version, scheme, commit, three downloads with a sha256 each, root')" || fail "$rel.env names every pin" "missing:$missing"
  # A key named twice would leave it to the reader which one wins: the Dockerfile and guest-build.sh refuse such a file, and none is one.
  dup="$(grep -E '^[A-Z0-9_]+=' "$f" | cut -d= -f1 | sort | uniq -d | tr '\n' ' ')"
  [[ -z "$dup" ]] && pass "$rel.env names no key twice" || fail "$rel.env names no key twice" "twice: $dup"
  # The guest tag is not a property of the ZisK release: it is a build argument of the image, so its label is the tag used.
  ! grep -q '^ROME_ZK_GUEST_TAG=' "$f" && pass "$rel.env has no default guest tag" || fail "$rel.env has no default guest tag" "$(grep ROME_ZK_GUEST_TAG "$f")"
  [[ "$(pin ZISK_VERSION)" == "$rel" ]] && pass "$rel.env: ZISK_VERSION is its own file name" || fail "$rel.env ZISK_VERSION" "$(pin ZISK_VERSION)"
  hashes_ok=1; for k in ZISKUP_SHA256 ZISK_TOOLCHAIN_SHA256 CARGO_ZISK_SHA256; do [[ "$(pin $k)" =~ ^[0-9a-f]{64}$ ]] || hashes_ok=0; done
  [[ $hashes_ok == 1 ]] && pass "$rel.env: the three download hashes are sha256 values" || fail "$rel.env hashes" "$(grep SHA256 "$f")"
  [[ "$(pin ROOT_C_VADCOP_FINAL)" =~ ^0x[0-9a-f]{64}$ ]] && [[ "$(pin ZISK_COMMIT)" =~ ^[0-9a-f]{40}$ ]] && [[ "$(pin ZISK_SCHEME)" =~ ^[0-9]+$ ]] && pass "$rel.env: root is 32 bytes of hex, the commit is 40 hex digits, the scheme is a number" || fail "$rel.env root, commit, scheme" "$(grep -E 'ROOT_C|COMMIT|SCHEME' "$f")"
  urls_ok=1; for k in ZISKUP_URL ZISK_TOOLCHAIN_URL CARGO_ZISK_URL; do case "$(pin $k)" in https://raw.githubusercontent.com/0xPolygonHermez/*|https://github.com/0xPolygonHermez/*) ;; *) urls_ok=0 ;; esac; done
  grep -q "/v$rel/" <<<"$(pin ZISKUP_URL)" && grep -q "/v$rel/" <<<"$(pin CARGO_ZISK_URL)" && grep -q "/$(pin ZISK_TOOLCHAIN_TAG)/" <<<"$(pin ZISK_TOOLCHAIN_URL)" || urls_ok=0
  [[ $urls_ok == 1 ]] && pass "$rel.env: every download is an https ZisK release URL of this release and toolchain tag" || fail "$rel.env URLs" "$(grep URL "$f")"
done
[[ "$(sed -n 's/^ZISK_SCHEME=//p' "$PINS/1.2.0-alpha.env")" == 1 && "$(sed -n 's/^ZISK_SCHEME=//p' "$PINS/1.3.1-alpha.env")" == 2 ]] && pass "scheme 1 is ZisK 1.2.0-alpha and scheme 2 is ZisK 1.3.1-alpha" || fail "scheme numbers" "$(grep -H SCHEME "$PINS"/*.env)"
[[ "$(sed -n 's/^ZISK_TOOLCHAIN_TAG=//p' "$PINS/1.2.0-alpha.env")" == zisk-3.0.0 && "$(sed -n 's/^ZISK_TOOLCHAIN_TAG=//p' "$PINS/1.3.1-alpha.env")" == zisk-4.0.0 ]] && pass "the Rust toolchain tags are zisk-3.0.0 (1.2.0-alpha) and zisk-4.0.0 (1.3.1-alpha)" || fail "toolchain tags" "$(grep -H TOOLCHAIN_TAG "$PINS"/*.env)"
[[ "$(sed -n 's/^ZISKUP_SHA256=//p' "$PINS/1.3.1-alpha.env")" == a2d1781f64f31719ef251f55a48c664ab35cdb7431d42167894f94b5fbec64e1 && "$(sed -n 's/^ROOT_C_VADCOP_FINAL=//p' "$PINS/1.3.1-alpha.env")" == 0xc3f12b9f8707c6a1e96df2bf6702c2ebdfbafedabeac654644a380befe091ac4 && "$(sed -n 's/^ZISK_COMMIT=//p' "$PINS/1.3.1-alpha.env")" == 306a9c934ba4947b1d586d69b67120f8b4c41466 ]] && pass "1.3.1-alpha: the installer hash, the vadcop_final root and the commit are the published ones" || fail "1.3.1-alpha installer, root and commit" "$(cat "$PINS/1.3.1-alpha.env")"
[[ "$(sed -n 's/^ROOT_C_VADCOP_FINAL=//p' "$PINS/1.2.0-alpha.env")" == 0x564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f ]] && pass "1.2.0-alpha: the vadcop_final root is unchanged" || fail "1.2.0-alpha root" "$(grep ROOT_C "$PINS/1.2.0-alpha.env")"
[[ "$(sed -n 's/^ROOT_C_VADCOP_FINAL=//p' "$PINS"/*.env | sort | uniq -d | wc -l | tr -d ' ')" == 0 ]] && pass "no two releases share a vadcop_final root" || fail "distinct roots" "$(grep ROOT_C "$PINS"/*.env)"
# The value of every pinned sha256, not only its shape: a changed hash must be a change of this test as well.
declare -A WANT=(
  [1.2.0-alpha:ZISKUP_SHA256]=7473ab9181d69727838ed45e129c045e8016038b5bd2963dbfabc23a382d5a43
  [1.2.0-alpha:ZISK_TOOLCHAIN_SHA256]=74190d265b2d8f10424cdf666b71a5b5670c3a685785cd668ee5f49318fa149e
  [1.2.0-alpha:CARGO_ZISK_SHA256]=e3605dafb5fcdb2fa8271ea945334694f42704737f31ef0bdf93d663f2b14866
  [1.3.1-alpha:ZISKUP_SHA256]=a2d1781f64f31719ef251f55a48c664ab35cdb7431d42167894f94b5fbec64e1
  [1.3.1-alpha:ZISK_TOOLCHAIN_SHA256]=c4c44b5612dd025f630c2f984ae5f8a862b885c4b14bfc57c980eb8073b8cf62
  [1.3.1-alpha:CARGO_ZISK_SHA256]=13e590784e79a195db7e85465eef75e0820be7148d12f829e478047542aadef8 )
wrong=""; for k in "${!WANT[@]}"; do got="$(sed -n "s/^${k#*:}=//p" "$PINS/${k%%:*}.env" | head -n 1)"; [[ "$got" == "${WANT[$k]}" ]] || wrong="$wrong ${k} is '$got';"; done
[[ -z "$wrong" ]] && pass "all six pinned sha256 values (installer, Rust toolchain, cargo_zisk tarball; both releases) are the published ones" || fail "pinned sha256 values" "$wrong"
grep -q 'sha256sum -c' "$DF" && grep -q 'pin ZISKUP_SHA256)  ' "$DF" && grep -q 'pin ZISK_TOOLCHAIN_SHA256)  ' "$DF" && grep -q 'pin CARGO_ZISK_SHA256)  ' "$DF" && grep -q 'ZISK_TOOLCHAIN_SOURCE_DIR=' "$DF" && pass "the Dockerfile checks the installer, the Rust toolchain tarball and the cargo_zisk tarball against the pin file's sha256 values" || fail "Dockerfile checks the three downloads" "$(grep -n 'sha256sum\|SHA256' "$DF")"
! grep -qE '^ARG (ZISKUP_SHA256|ZISK_TOOLCHAIN_SHA256|CARGO_ZISK_SHA256)=' "$DF" && pass "no hash is set in the Dockerfile itself; the pin file is the one place" || fail "hashes live in the pin files only" "$(grep -n '^ARG' "$DF")"
# The toolchain tag is an argument only so that it can be a label of the image; the build stops unless it is the pin file's.
def_tag="$(sed -n 's/^ARG ZISK_TOOLCHAIN_TAG=//p' "$DF")"; def_rel="$(sed -n 's/^ARG ZISK_RELEASE=//p' "$DF")"
[[ -n "$def_tag" && "$def_tag" == "$(sed -n 's/^ZISK_TOOLCHAIN_TAG=//p' "$PINS/$def_rel.env")" ]] && grep -q 'ToolchainTagMismatch' "$DF" && pass "the toolchain tag argument defaults to the default release's tag from its pin file, and the build refuses (ToolchainTagMismatch) when it is not the pin file's" || fail "toolchain tag argument" "$(grep -n 'TOOLCHAIN_TAG' "$DF")"
grep -q 'COPY zisk/ ' "$DF" && grep -q '/usr/local/share/guest-build/release' "$DF" && pass "the image carries the pin files and writes its release to a file, for guest-build to read" || fail "pin files and the release file in the image" "$(grep -n 'COPY\|guest-build/release' "$DF")"
! grep -E '^ENV ' "$DF" | grep -qE 'ZISK_VERSION|ROME_ZK_' && pass "the release and the source tags are not environment variables of the image: the environment of a container cannot relabel them" || fail "no release or tag in ENV" "$(grep -n '^ENV' "$DF")"
grep -q '/work/rome-zk-evm.tag' "$DF" && grep -q '/work/rome-zk-guest.tag' "$DF" && pass "the image records the two source tags it cloned in files next to the revisions" || fail "tag files" "$(grep -n '\.tag' "$DF")"
clone_line="$(grep -n 'git clone' "$DF" | tail -n 1 | cut -d: -f1)"; check_line="$(grep -n 'check-guest-lock "' "$DF" | head -n 1 | cut -d: -f1)"
[[ -n "$clone_line" && -n "$check_line" && "$check_line" -gt "$clone_line" ]] && grep -q 'COPY check-guest-lock.sh ' "$DF" && grep -q 'stateless-validator-rome/Cargo.lock' "$DF" && pass "right after the clone, the image checks the guest's Cargo.lock against its release (check-guest-lock)" || fail "the lock check after the clone" "clone line ${clone_line:-none}, check line ${check_line:-none}"
labels="$(sed -n '/^LABEL /,$p' "$DF" | sed '/^$/q')"
for l in 'zisk.version="${ZISK_RELEASE}"' 'zisk.toolchain-tag="${ZISK_TOOLCHAIN_TAG}"' 'rome-zk-evm.tag="${ROME_ZK_EVM_TAG}"' 'rome-zk-guest.tag="${ROME_ZK_GUEST_TAG}"'; do
  grep -qF "$l" <<<"$labels" || fail "image label $l" "$labels"
done
grep -qF 'zisk.toolchain-tag=' <<<"$labels" && grep -qF 'rome-zk-guest.tag="${ROME_ZK_GUEST_TAG}"' <<<"$labels" && pass "the image labels record the ZisK release, the toolchain tag and the two source tags that were used"
# Every value of a pin file is the first line of its key, in the Dockerfile and in the image's script.
grep -q 'sed -n "s/^\$1=//p" .* | head -n 1' "$DF" && grep -q '^pin() { sed -n "s/^\$2=//p" "\$1" | head -n 1; }' "$GB_DIR/guest-build.sh" && pass "pin() takes the first line of a key, in the Dockerfile and in guest-build.sh" || fail "pin() takes the first line" "$(grep -n 'pin()' "$DF" "$GB_DIR/guest-build.sh")"
grep -q 'PinFileInvalid' "$DF" && grep -q 'PinFileInvalid' "$GB_DIR/guest-build.sh" && pass "both refuse a pin file that names a key twice (PinFileInvalid)" || fail "PinFileInvalid in both" "$(grep -c PinFileInvalid "$DF" "$GB_DIR/guest-build.sh")"
# The example in the Dockerfile's header names tags that exist: the ones the command itself defaults to.
ex_evm="$(sed -n 's/.*--build-arg ROME_ZK_EVM_TAG=\([^ ]*\).*/\1/p' "$DF" | head -n 1)"; ex_guest="$(sed -n 's/.*--build-arg ROME_ZK_GUEST_TAG=\([^ ]*\).*/\1/p' "$DF" | head -n 1)"
[[ "$ex_evm" == "$(sed -n 's/^GUEST_BUILD_EVM_TAG_DEFAULT=//p' "$ROLLUP")" && "$ex_guest" == "$(sed -n 's/^GUEST_BUILD_GUEST_TAG_DEFAULT=//p' "$ROLLUP")" ]] && pass "the example in the Dockerfile's header uses the published tags the command defaults to ($ex_evm, $ex_guest)" || fail "the Dockerfile header example" "evm '$ex_evm' guest '$ex_guest'"
grep -q -- '--nokey' "$GB_DIR/Dockerfile" && ! grep -q -- '--provingkey' "$GB_DIR/Dockerfile" && pass "the image holds no proving keys" || fail "no proving keys in the image" "$(grep -n 'ziskup -v' "$GB_DIR/Dockerfile")"
# The node tag in .env decides the node-side source; the guest tag has its own setting.
reset_calls
printf 'ROME_ZK_TAG=v9.9.9\nROME_ZK_GUEST_TAG=v8.8.8\n' >> "$WORK/rollup.env"
env -u ROME_ZK_IMAGE "$ROLLUP" guest-build >/dev/null 2>&1
grep -q 'ROME_ZK_EVM_TAG=v9.9.9' "$WORK/build_calls" && grep -q 'ROME_ZK_GUEST_TAG=v8.8.8' "$WORK/build_calls" && grep -q -- '-t rome-zk-guest-build:1.3.1-alpha-v9.9.9-v8.8.8 ' "$WORK/build_calls" && pass "ROME_ZK_TAG and ROME_ZK_GUEST_TAG in .env choose the sources" || fail "tags from .env" "$(cat "$WORK/build_calls")"
sed -i.bak '/^ROME_ZK_TAG=/d;/^ROME_ZK_GUEST_TAG=/d' "$WORK/rollup.env"; rm -f "$WORK/rollup.env.bak"
# ZISK_RELEASE in .env chooses the release: the image is built for it, takes its toolchain tag from its pin file and carries
# its name, so two releases never share an image tag.
reset_calls
printf 'ZISK_RELEASE=1.2.0-alpha\n' >> "$WORK/rollup.env"
"$ROLLUP" guest-build >/dev/null 2>&1
grep -q -- '--build-arg ZISK_RELEASE=1.2.0-alpha ' "$WORK/build_calls" && grep -q -- '--build-arg ZISK_TOOLCHAIN_TAG=zisk-3.0.0 ' "$WORK/build_calls" && grep -q -- '-t rome-zk-guest-build:1.2.0-alpha-v0.3.0-v0.3.0 ' "$WORK/build_calls" && pass "ZISK_RELEASE=1.2.0-alpha builds rome-zk-guest-build:1.2.0-alpha-<node tag>-<guest tag> with toolchain tag zisk-3.0.0" || fail "ZISK_RELEASE from .env" "$(cat "$WORK/build_calls")"
# A release with no pin file, and a value that is not a plain release name, are refused before docker runs. The third value
# names a pin file that exists (zisk/../zisk/1.3.1-alpha.env), so only the name check can refuse it: the test reaches the regex.
[[ -f "$GB_DIR/zisk/../zisk/1.3.1-alpha.env" ]] && pass "test setup: the traversal value names a pin file that exists" || fail "test setup: traversal pin file" "$GB_DIR/zisk/../zisk/1.3.1-alpha.env is missing"
for bad in 9.9.9-alpha '../1.3.1-alpha' '../zisk/1.3.1-alpha' '1.3.1-alpha --privileged'; do
  reset_calls
  sed -i.bak '/^ZISK_RELEASE=/d' "$WORK/rollup.env"; rm -f "$WORK/rollup.env.bak"; printf 'ZISK_RELEASE=%s\n' "$bad" >> "$WORK/rollup.env"
  refuses ZiskReleaseUnknown
done
sed -i.bak '/^ZISK_RELEASE=/d' "$WORK/rollup.env"; rm -f "$WORK/rollup.env.bak"

# ---- with keys ------------------------------------------------------------------------------------------------------------
mkdir -p "$WORK/zh/provingKey"
printf 'ZISK_HOME=%s\n' "$WORK/zh" >> "$WORK/rollup.env"
reset_calls
out="$("$ROLLUP" guest-build 2>"$WORK/stderr")"; rc=$?
[[ $rc -eq 0 ]] && pass "with ZISK_HOME the build succeeds" || fail "with ZISK_HOME the build succeeds" "exit $rc: $(cat "$WORK/stderr")"
grep -q "source=$WORK/zh/provingKey,target=/keys/provingKey,readonly$" "$WORK/mounts" && pass "the proving keys are mounted read-only" || fail "keys mount" "$(cat "$WORK/mounts")"
grep -q -- '--proving-key-dir /keys/provingKey' "$WORK/run_calls" && pass "the image is told where the keys are" || fail "--proving-key-dir" "$(cat "$WORK/run_calls")"
grep -q "^program VK *0xabab" <<<"$out" && pass "reports the programVK" || fail "reports the programVK" "$out"
grep -q ProgramVkNotComputed "$WORK/stderr" && fail "no ProgramVkNotComputed when the programVK is there" "$(cat "$WORK/stderr")" || pass "no ProgramVkNotComputed when the programVK is there"
# --skip-program-vk
reset_calls
"$ROLLUP" guest-build --skip-program-vk >/dev/null 2>"$WORK/stderr"
grep -q 'proving-key-dir' "$WORK/run_calls" && fail "--skip-program-vk passes no keys" "$(cat "$WORK/run_calls")" || pass "--skip-program-vk passes no keys"
grep -q '^ProgramVkNotComputed' "$WORK/stderr" && pass "--skip-program-vk says ProgramVkNotComputed" || fail "--skip-program-vk message" "$(cat "$WORK/stderr")"
# ---- failures the build reports ---------------------------------------------------------------------------------------------
reset_calls; touch "$WORK/stub_fail"
if out="$("$ROLLUP" guest-build 2>&1)"; then fail "a failed build exits non-zero" "exited 0"
elif grep -q '^GuestBuildFailed: the build in ' <<<"$out" && grep -q 'stub build failure' <<<"$out"; then pass "a failed build exits non-zero with GuestBuildFailed and the image's own reason"; else fail "GuestBuildFailed" "$out"; fi
reset_calls
if out="$(STUB_BAD_SHA=$(printf 'ee%.0s' $(seq 32)) "$ROLLUP" guest-build 2>&1)"; then fail "an ELF that does not hash to its name is refused" "exited 0"
elif grep -q '^GuestBuildFailed' <<<"$out"; then pass "an ELF that does not hash to the reported sha256 is refused (GuestBuildFailed)"; else fail "bad sha refused" "$out"; fi
# The command never edits the rendered genesis or the chain id.
cmp -s "$GOOD_GENESIS" "$WORK/out/genesis.json" && pass "rendered/genesis.json is untouched" || fail "rendered/genesis.json is untouched" "changed"
finish guest_build_command
