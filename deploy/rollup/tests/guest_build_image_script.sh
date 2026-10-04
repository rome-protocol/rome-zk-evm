#!/usr/bin/env bash
# deploy/rollup/tests/guest_build_image_script.sh — the script that runs inside the guest-build image
# (deploy/rollup/guest-build/guest-build.sh), with a stub build script and a stub cargo-zisk: the refusals it carries out by
# name, the ELF it names by hash, and the vkey.json it writes in the format rome-zk-prover reads, with the programVK read
# from cargo-zisk setup when the keys are there and a named message when they are not.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
SCRIPT="$ROLLUP_DIR/guest-build/guest-build.sh"

IN="$WORK/in"; OUTD="$WORK/outd"; WK="$WORK/work"; KEYS="$WORK/keys/provingKey"; BIN="$WORK/bin"
GUEST="$WK/rome-zk-evm/.fork/bin/guests/stateless-validator-rome"
mkdir -p "$IN" "$OUTD" "$GUEST" "$KEYS/zisk/vadcop_final" "$BIN"
echo '{"config":{"chainId":4295391538},"alloc":{}}' > "$IN/genesis.json"
echo deadbeefdeadbeefdeadbeefdeadbeefdeadbeef > "$WK/rome-zk-evm.rev"; echo cafebabecafebabecafebabecafebabecafebabe > "$WK/rome-zk-guest.rev"
# The real vadcop_final verification key of the ZisK 1.2.0-alpha key set.
echo '[6218392583875695404, 9353885302538021251, 8280779842059074605, 10684020678174455855]' > "$KEYS/zisk/vadcop_final/vadcop_final.verkey.json"
printf 'GENESIS-STUB' > "$WORK/elf_body"
ELF_SHA="$(python3 -c 'import hashlib; print(hashlib.sha256(b"ELF-FOR-STUB-GUEST").hexdigest())')"

# The stub build script prints what the real one prints; $WORK/build_mode picks a refusal.
cat > "$GUEST/build-elf.sh" <<STUB
#!/usr/bin/env bash
mode="\$(cat "$WORK/build_mode" 2>/dev/null || echo ok)"
case "\$mode" in
  chain) echo "build-elf.sh: REFUSING — chain id mismatch: 'g' has config.chainId 1 but --expect-chain-id is 2." >&2; exit 1 ;;
  funded) echo "guest-rome build.rs: REFUSING — g: GenesisFundedAccountLimit: the genesis gives 2 accounts a non-zero balance (a, b); at most one is allowed" >&2; exit 1 ;;
  crash) echo "error: something else" >&2; exit 101 ;;
esac
printf 'ELF-FOR-STUB-GUEST' > "$WORK/stub.elf"
echo "build-elf.sh: built $WORK/stub.elf"
echo "build-elf.sh: sha256=$ELF_SHA"
echo "build-elf.sh: genesis_sha256=\$(printf 'a%.0s' \$(seq 64))"
echo "build-elf.sh: chain_id=\${STUB_CHAIN_ID:-4295391538}"
echo "build-elf.sh: genesis_balance=none"
STUB
chmod +x "$GUEST/build-elf.sh"
# The stub cargo-zisk records its call and prints the root hash the real 1.2.0-alpha printed for the example ELF.
cat > "$BIN/cargo-zisk" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$WORK/zisk_calls"
[ ! -f "$WORK/zisk_fail" ] || { echo "Killed" >&2; exit 137; }
echo "INFO: Root hash: [4780737295883886437, 10316318635792104790, 4509567273278372526, 13776656993784749175]"
STUB
chmod +x "$BIN/cargo-zisk"
export PATH="$BIN:$PATH"
export GUEST_BUILD_IN="$IN" GUEST_BUILD_OUT="$OUTD" GUEST_BUILD_WORK="$WK" ROME_ZK_EVM_TAG=v0.2.1 ROME_ZK_GUEST_TAG=v0.2.0
printf 'MemTotal:       131072000 kB\n' > "$WORK/meminfo"; export GUEST_BUILD_MEMINFO="$WORK/meminfo"
reset() { rm -f "$OUTD"/* "$WORK/zisk_calls" "$WORK/zisk_fail" "$WORK/build_mode"; : > "$WORK/zisk_calls"; }
run() { OUT_TEXT="$("$SCRIPT" "$@" 2>"$WORK/err")"; RC=$?; ERR="$(cat "$WORK/err")"; }
refused() { # $1=name $2=label -> exit non-zero, stderr starts with the name, no ELF written
  [[ $RC -ne 0 ]] && grep -q "^$1" "$WORK/err" && ! ls "$OUTD"/*.elf >/dev/null 2>&1 && pass "$2: refused by name ($1), no ELF written" || fail "$2: refused by name ($1)" "rc=$RC err=$ERR files=$(ls "$OUTD")"
}
# What rome-zk-prover's vkey loader needs (crates/rome-zk-prover/src/config.rs): programVK, rootCVadcopFinal and elf_sha256 as
# 32 bytes of hex (0x optional), chain_id a number, layout_id 1.
loader_accepts() { python3 - "$1" "$2" "$3" <<'PY'
import json, sys
path, chain, sha = sys.argv[1:]
d = json.load(open(path))
def h32(k):
    v = d[k]; assert isinstance(v, str); b = bytes.fromhex(v[2:] if v.startswith("0x") else v); assert len(b) == 32, k
for k in ("programVK", "rootCVadcopFinal", "elf_sha256"): h32(k)
assert isinstance(d["chain_id"], int) and d["chain_id"] == int(chain)
assert d["layout_id"] == 1 and d["elf_sha256"] == sha
PY
}

# ---- refusals --------------------------------------------------------------------------------------------------------------
reset; run;                                   refused ChainIdMissing "no --expect-chain-id"
reset; run --expect-chain-id abc;             refused ChainIdMissing "a chain id that is not a number"
reset; run --expect-chain-id 4295391538 --colour; refused UnknownArgument "an unknown argument"
reset; run --expect-chain-id 4295391538 --proving-key-dir; refused FlagValueMissing "a key dir flag without a value"
reset; echo chain > "$WORK/build_mode"; run --expect-chain-id 4295391538;  refused ChainIdMismatch "the build script's chain id refusal"
reset; echo funded > "$WORK/build_mode"; run --expect-chain-id 4295391538; refused GenesisFundedAccountLimit "a genesis with two funded accounts"
grep -q 'at most one is allowed' <<<"$ERR" && pass "the funded-account refusal carries the build's own reason" || fail "funded-account reason" "$ERR"
reset; echo crash > "$WORK/build_mode"; run --expect-chain-id 4295391538; refused GuestBuildFailed "a build that fails for another reason"
reset; STUB_CHAIN_ID=7 run --expect-chain-id 4295391538; refused ChainIdMismatch "a build that embeds another chain id than expected"
reset; mv "$IN/genesis.json" "$IN/g.bak"; run --expect-chain-id 4295391538; refused GenesisMissing "no genesis mounted"; mv "$IN/g.bak" "$IN/genesis.json"
reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/nokeys"; refused ProvingKeyDirMissing "a key dir that is not there"
mkdir -p "$WORK/badkeys/zisk"; reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/badkeys"; refused ProvingKeyInvalid "a key dir without the vadcop_final key"
mkdir -p "$WORK/otherkeys/zisk/vadcop_final"; echo '[1,2,3,4]' > "$WORK/otherkeys/zisk/vadcop_final/vadcop_final.verkey.json"
reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/otherkeys"; refused ProvingKeyMismatch "keys of another release"
[[ ! -s "$WORK/zisk_calls" ]] && pass "cargo-zisk did not run on keys of another release" || fail "cargo-zisk not run on a wrong key set" "$(cat "$WORK/zisk_calls")"

# ---- no keys: the ELF, and a vkey.json without programVK ------------------------------------------------------------------------
reset; run --expect-chain-id 4295391538
[[ $RC -eq 0 ]] && pass "without keys the build succeeds" || fail "without keys the build succeeds" "rc=$RC $ERR"
cmp -s "$OUTD/$ELF_SHA.elf" "$WORK/stub.elf" && pass "the ELF is written as <sha256>.elf" || fail "the ELF is written as <sha256>.elf" "$(ls "$OUTD")"
[[ "$(jq -r 'has("programVK")' "$OUTD/vkey.json")" == false ]] && pass "vkey.json has no programVK" || fail "vkey.json has no programVK" "$(cat "$OUTD/vkey.json")"
grep -q '^ProgramVkNotComputed: no ZisK proving key directory' <<<"$ERR" && pass "the missing programVK is named: ProgramVkNotComputed" || fail "ProgramVkNotComputed" "$ERR"
grep -q '^program_vk=none$' <<<"$OUT_TEXT" && grep -q "^elf_sha256=$ELF_SHA$" <<<"$OUT_TEXT" && pass "prints elf_sha256 and program_vk=none" || fail "printed lines" "$OUT_TEXT"
[[ ! -s "$WORK/zisk_calls" ]] && pass "cargo-zisk is not run without keys" || fail "cargo-zisk not run without keys" "$(cat "$WORK/zisk_calls")"
[[ "$(jq -r '.rootCVadcopFinal, .elf_sha256, .chain_id, .layout_id' "$OUTD/vkey.json" | tr '\n' ' ')" == "0x564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f $ELF_SHA 4295391538 1 " ]] && pass "vkey.json carries the key set's root, the ELF sha256, the chain id and layout 1" || fail "vkey.json fields" "$(cat "$OUTD/vkey.json")"

# ---- with keys: the programVK from cargo-zisk setup -e ---------------------------------------------------------------------------
reset; run --expect-chain-id 4295391538 --proving-key-dir "$KEYS"
[[ $RC -eq 0 ]] && pass "with keys the build succeeds" || fail "with keys the build succeeds" "rc=$RC $ERR"
grep -q "^setup -e $OUTD/$ELF_SHA.elf -k $KEYS\$" "$WORK/zisk_calls" && pass "runs cargo-zisk setup -e on the ELF with the key directory (no proof)" || fail "cargo-zisk call" "$(cat "$WORK/zisk_calls")"
[[ "$(jq -r .programVK "$OUTD/vkey.json")" == 0x4258973dbd9edf658f2aed241c217d563e95338ad67bfaaebf30848b42f0dc77 ]] && pass "programVK is the four root-hash limbs as one big-endian number (0x4258973d…dc77 for the example)" || fail "programVK" "$(cat "$OUTD/vkey.json")"
loader_accepts "$OUTD/vkey.json" 4295391538 "$ELF_SHA" && pass "vkey.json has every field and shape rome-zk-prover's loader needs" || fail "loader format" "$(cat "$OUTD/vkey.json")"
grep -q ProgramVkNotComputed <<<"$ERR" && fail "no ProgramVkNotComputed with a programVK" "$ERR" || pass "no ProgramVkNotComputed with a programVK"
# the fixture the prover's own tests load has the same fields
for f in "$ROLLUP_DIR"/../../fixtures/vkeys/*-layout1.json; do [[ -f "$f" ]] || continue
  for k in programVK rootCVadcopFinal elf_sha256 chain_id layout_id; do jq -e "has(\"$k\")" "$f" >/dev/null 2>&1 || fail "fixture has $k" "missing"; done
done

# ---- too little memory, and a failed setup -----------------------------------------------------------------------------------------
reset; printf 'MemTotal:       16777216 kB\n' > "$WORK/meminfo"; run --expect-chain-id 4295391538 --proving-key-dir "$KEYS"
[[ $RC -eq 0 ]] && grep -q '^ProgramVkNotComputed: this machine has 16 GiB' <<<"$ERR" && pass "with 16 GiB the programVK step is skipped, by name, with the reason" || fail "low memory" "rc=$RC $ERR"
[[ ! -s "$WORK/zisk_calls" && "$(jq -r 'has("programVK")' "$OUTD/vkey.json")" == false ]] && pass "no setup was tried and vkey.json has no programVK" || fail "low memory: nothing tried" "$(cat "$WORK/zisk_calls")"
printf 'MemTotal:       131072000 kB\n' > "$WORK/meminfo"
reset; touch "$WORK/zisk_fail"; run --expect-chain-id 4295391538 --proving-key-dir "$KEYS"
[[ $RC -ne 0 ]] && grep -q '^ProgramVkFailed' "$WORK/err" && pass "a failed cargo-zisk setup stops with ProgramVkFailed" || fail "ProgramVkFailed" "rc=$RC $ERR"
finish guest_build_image_script
