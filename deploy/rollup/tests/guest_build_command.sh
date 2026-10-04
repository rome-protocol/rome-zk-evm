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
grep -q -- '-t rome-zk-guest-build:v0.2.2-v0.2.0 ' "$WORK/build_calls" && grep -q 'ROME_ZK_EVM_TAG=v0.2.2' "$WORK/build_calls" && grep -q 'ROME_ZK_GUEST_TAG=v0.2.0' "$WORK/build_calls" && pass "default tags: node v0.2.2, guest v0.2.0" || fail "default tags" "$(cat "$WORK/build_calls")"
grep -q 'guest-build$' "$WORK/build_calls" && pass "the build context is deploy/rollup/guest-build" || fail "build context" "$(cat "$WORK/build_calls")"
# The Dockerfile's own defaults are the same tags, so a by-hand build gives the same image.
grep -q '^ARG ROME_ZK_EVM_TAG=v0.2.2$' "$GB_DIR/Dockerfile" && grep -q '^ARG ROME_ZK_GUEST_TAG=v0.2.0$' "$GB_DIR/Dockerfile" && pass "the Dockerfile defaults are the same tags" || fail "Dockerfile defaults" "$(grep '^ARG' "$GB_DIR/Dockerfile")"
grep -q '^FROM ubuntu@sha256:[0-9a-f]\{64\}$' "$GB_DIR/Dockerfile" && pass "the base image is pinned by digest" || fail "base image pinned by digest" "$(grep '^FROM' "$GB_DIR/Dockerfile")"
grep -q 'ZISK_VERSION=1.2.0-alpha' "$GB_DIR/Dockerfile" && grep -q 'sha256sum -c' "$GB_DIR/Dockerfile" && pass "ZisK 1.2.0-alpha, with its installer checked against a sha256" || fail "ZisK pin" "$(grep -n ZISK "$GB_DIR/Dockerfile")"
DF="$GB_DIR/Dockerfile"
grep -q '^ARG ZISK_TOOLCHAIN_TAG=zisk-3\.0\.0$' "$DF" && grep -q '^ARG ZISK_TOOLCHAIN_SHA256=[0-9a-f]\{64\}$' "$DF" && grep -q '^ARG CARGO_ZISK_SHA256=[0-9a-f]\{64\}$' "$DF" && grep -q 'ZISK_TOOLCHAIN_SOURCE_DIR=' "$DF" && grep -q '"\${ZISK_TOOLCHAIN_SHA256}  ' "$DF" && grep -q '"\${CARGO_ZISK_SHA256}  ' "$DF" && pass "the ZisK Rust toolchain is pinned to a tag, and its tarball and the cargo_zisk tarball are checked against sha256 values" || fail "toolchain tag and both tarball hashes pinned" "$(grep -n 'ZISK_TOOLCHAIN\|CARGO_ZISK_SHA' "$DF")"
grep -q -- '--nokey' "$GB_DIR/Dockerfile" && ! grep -q -- '--provingkey' "$GB_DIR/Dockerfile" && pass "the image holds no proving keys" || fail "no proving keys in the image" "$(grep -n 'ziskup -v' "$GB_DIR/Dockerfile")"
# The node tag in .env decides the node-side source; the guest tag has its own setting.
reset_calls
printf 'ROME_ZK_TAG=v9.9.9\nROME_ZK_GUEST_TAG=v8.8.8\n' >> "$WORK/rollup.env"
env -u ROME_ZK_IMAGE "$ROLLUP" guest-build >/dev/null 2>&1
grep -q 'ROME_ZK_EVM_TAG=v9.9.9' "$WORK/build_calls" && grep -q 'ROME_ZK_GUEST_TAG=v8.8.8' "$WORK/build_calls" && grep -q -- '-t rome-zk-guest-build:v9.9.9-v8.8.8 ' "$WORK/build_calls" && pass "ROME_ZK_TAG and ROME_ZK_GUEST_TAG in .env choose the sources" || fail "tags from .env" "$(cat "$WORK/build_calls")"
sed -i.bak '/^ROME_ZK_TAG=/d;/^ROME_ZK_GUEST_TAG=/d' "$WORK/rollup.env"; rm -f "$WORK/rollup.env.bak"

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
