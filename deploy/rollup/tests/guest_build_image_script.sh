#!/usr/bin/env bash
# deploy/rollup/tests/guest_build_image_script.sh — the script that runs inside the guest-build image
# (deploy/rollup/guest-build/guest-build.sh), with a stub build script and a stub cargo-zisk: the refusals it carries out by
# name, the ELF it names by hash, and the vkey.json it writes in the format rome-zk-prover reads, with the programVK read
# from cargo-zisk setup when the keys are there and a named message when they are not. The ZisK release comes from a file
# the image was built with and its pin file under guest-build/zisk/, never from the environment of the container; the
# cargo-zisk in the image must be that release at the pin file's commit. The main run is for 1.3.1-alpha and the end of the
# test runs the 1.2.0-alpha image and the mix-ups between the two.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
SCRIPT="$ROLLUP_DIR/guest-build/guest-build.sh"

IN="$WORK/in"; OUTD="$WORK/outd"; WK="$WORK/work"; KEYS="$WORK/keys/provingKey"; BIN="$WORK/bin"
GUEST="$WK/rome-zk-evm/.fork/bin/guests/stateless-validator-rome"
mkdir -p "$IN" "$OUTD" "$GUEST" "$KEYS/zisk/vadcop_final" "$BIN"
echo '{"config":{"chainId":4295391538},"alloc":{}}' > "$IN/genesis.json"
echo deadbeefdeadbeefdeadbeefdeadbeefdeadbeef > "$WK/rome-zk-evm.rev"; echo cafebabecafebabecafebabecafebabecafebabe > "$WK/rome-zk-guest.rev"
# What the image bakes in at build time: the release, and the two source tags that were cloned.
RELEASE_FILE="$WORK/release"; echo 1.3.1-alpha > "$RELEASE_FILE"
echo v0.2.2 > "$WK/rome-zk-evm.tag"; echo v0.3.1 > "$WK/rome-zk-guest.tag"
COMMIT_1_3_1=306a9c9; COMMIT_1_2_0=fbbc69b
# The vadcop_final verification key of each ZisK release's key set, as the four limbs of vadcop_final.verkey.json.
KEY_1_3_1='[14119114270948443809, 16820367087179580139, 16121478031581406534, 4945938373577087684]'
KEY_1_2_0='[6218392583875695404, 9353885302538021251, 8280779842059074605, 10684020678174455855]'
ROOT_1_3_1=0xc3f12b9f8707c6a1e96df2bf6702c2ebdfbafedabeac654644a380befe091ac4
ROOT_1_2_0=0x564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f
echo "$KEY_1_3_1" > "$KEYS/zisk/vadcop_final/vadcop_final.verkey.json"
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
# The stub cargo-zisk answers --version with the line in $WORK/zisk_version (the image's own cargo-zisk), records every other
# call and prints the root hash the real cargo-zisk printed for the example ELF.
cat > "$BIN/cargo-zisk" <<STUB
#!/usr/bin/env bash
if [ "\$*" = "--version" ]; then cat "$WORK/zisk_version" 2>/dev/null || exit 127; exit 0; fi
echo "\$*" >> "$WORK/zisk_calls"
[ ! -f "$WORK/zisk_fail" ] || { echo "Killed" >&2; exit 137; }
echo "INFO: Root hash: [4780737295883886437, 10316318635792104790, 4509567273278372526, 13776656993784749175]"
STUB
chmod +x "$BIN/cargo-zisk"
export PATH="$BIN:$PATH"
# The environment of the container says other things than the image does: none of it may change what the script writes.
export GUEST_BUILD_IN="$IN" GUEST_BUILD_OUT="$OUTD" GUEST_BUILD_WORK="$WK" GUEST_BUILD_RELEASE_FILE="$RELEASE_FILE"
export GUEST_BUILD_PINS="$ROLLUP_DIR/guest-build/zisk"
export ZISK_VERSION=1.2.0-alpha ROME_ZK_EVM_TAG=v9.9.9 ROME_ZK_GUEST_TAG=v8.8.8
cargo_says() { printf 'cargo-zisk %s (%s 2026-05-12T10:00:00Z)\n' "$1" "$2" > "$WORK/zisk_version"; }
cargo_says 1.3.1-alpha "$COMMIT_1_3_1"
printf 'MemTotal:       131072000 kB\n' > "$WORK/meminfo"; export GUEST_BUILD_MEMINFO="$WORK/meminfo"
reset() { rm -f "$OUTD"/* "$WORK/zisk_calls" "$WORK/zisk_fail" "$WORK/build_mode"; : > "$WORK/zisk_calls"; }
is_release() { echo "$1" > "$RELEASE_FILE"; }
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
reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/otherkeys"; refused ProvingKeyMismatch "keys that are of no release"
mkdir -p "$WORK/oldkeys/zisk/vadcop_final"; echo "$KEY_1_2_0" > "$WORK/oldkeys/zisk/vadcop_final/vadcop_final.verkey.json"
reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/oldkeys"; refused ProvingKeyMismatch "the 1.2.0-alpha keys given to the 1.3.1-alpha image"
grep -q '1\.2\.0-alpha' <<<"$ERR" && grep -q '1\.3\.1-alpha' <<<"$ERR" && pass "the refusal names the release the keys are of and the release the image builds for" || fail "the refusal names both releases" "$ERR"
[[ ! -s "$WORK/zisk_calls" ]] && pass "cargo-zisk did not run on keys of another release" || fail "cargo-zisk not run on a wrong key set" "$(cat "$WORK/zisk_calls")"
is_release 9.9.9-alpha; reset; run --expect-chain-id 4295391538; refused ZiskReleaseUnknown "an image whose release has no pin file"
is_release 1.3.1-alpha
mv "$RELEASE_FILE" "$RELEASE_FILE.bak"; reset; run --expect-chain-id 4295391538; refused ZiskReleaseUnknown "an image with no release file"; mv "$RELEASE_FILE.bak" "$RELEASE_FILE"
is_release ../zisk/1.3.1-alpha; reset; run --expect-chain-id 4295391538; refused ZiskReleaseUnknown "a release written as a path"
is_release 1.3.1-alpha
# The release is the image's, not the container's environment: `docker run -e ZISK_VERSION=...` relabels nothing.
reset; ZISK_VERSION=1.2.0-alpha run --expect-chain-id 4295391538
[[ $RC -eq 0 && "$(jq -r '.zisk, .scheme, .rootCVadcopFinal' "$OUTD/vkey.json" | tr '\n' ' ')" == "1.3.1-alpha 2 $ROOT_1_3_1 " ]] && pass "ZISK_VERSION in the environment of the container changes nothing: vkey.json still says 1.3.1-alpha, scheme 2" || fail "ZISK_VERSION in the environment is ignored" "rc=$RC $ERR $(cat "$OUTD/vkey.json" 2>/dev/null)"
# The cargo-zisk of the image must be the release, at the pin file's commit.
cargo_says 1.2.0-alpha "$COMMIT_1_2_0"; reset; run --expect-chain-id 4295391538
refused ZiskToolchainMismatch "a cargo-zisk of another release than the image's"
grep -q '1\.2\.0-alpha' <<<"$ERR" && grep -q '1\.3\.1-alpha' <<<"$ERR" && pass "that refusal names the release cargo-zisk reports and the release of the image" || fail "toolchain refusal names both releases" "$ERR"
cargo_says 1.3.1-alpha "$COMMIT_1_2_0"; reset; run --expect-chain-id 4295391538; refused ZiskToolchainMismatch "the right release at another commit"
cargo_says 1.3.1-alpha-rc1 "$COMMIT_1_3_1"; reset; run --expect-chain-id 4295391538; refused ZiskToolchainMismatch "a release that only starts like the image's"
mv "$WORK/zisk_version" "$WORK/zisk_version.bak"; reset; run --expect-chain-id 4295391538; refused ZiskToolchainMismatch "a cargo-zisk that does not report a version"; mv "$WORK/zisk_version.bak" "$WORK/zisk_version"
cargo_says 1.3.1-alpha "$COMMIT_1_3_1"
# The pin file is read as the first line of each key; a key twice is refused rather than one of them picked.
mkdir -p "$WORK/dup"; cp "$ROLLUP_DIR"/guest-build/zisk/*.env "$WORK/dup/"; echo 'ZISK_SCHEME=7' >> "$WORK/dup/1.3.1-alpha.env"
reset; GUEST_BUILD_PINS="$WORK/dup" run --expect-chain-id 4295391538; refused PinFileInvalid "a pin file that names ZISK_SCHEME twice"
grep -q 'ZISK_SCHEME' <<<"$ERR" && pass "that refusal names the key" || fail "PinFileInvalid names the key" "$ERR"

# ---- no keys: the ELF, and a vkey.json without programVK ------------------------------------------------------------------------
reset; run --expect-chain-id 4295391538
[[ $RC -eq 0 ]] && pass "without keys the build succeeds" || fail "without keys the build succeeds" "rc=$RC $ERR"
cmp -s "$OUTD/$ELF_SHA.elf" "$WORK/stub.elf" && pass "the ELF is written as <sha256>.elf" || fail "the ELF is written as <sha256>.elf" "$(ls "$OUTD")"
[[ "$(jq -r 'has("programVK")' "$OUTD/vkey.json")" == false ]] && pass "vkey.json has no programVK" || fail "vkey.json has no programVK" "$(cat "$OUTD/vkey.json")"
grep -q '^ProgramVkNotComputed: no ZisK proving key directory' <<<"$ERR" && pass "the missing programVK is named: ProgramVkNotComputed" || fail "ProgramVkNotComputed" "$ERR"
grep -q '^program_vk=none$' <<<"$OUT_TEXT" && grep -q "^elf_sha256=$ELF_SHA$" <<<"$OUT_TEXT" && pass "prints elf_sha256 and program_vk=none" || fail "printed lines" "$OUT_TEXT"
[[ ! -s "$WORK/zisk_calls" ]] && pass "cargo-zisk is not run without keys" || fail "cargo-zisk not run without keys" "$(cat "$WORK/zisk_calls")"
[[ "$(jq -r '.rootCVadcopFinal, .elf_sha256, .chain_id, .layout_id' "$OUTD/vkey.json" | tr '\n' ' ')" == "$ROOT_1_3_1 $ELF_SHA 4295391538 1 " ]] && pass "vkey.json carries the key set's root, the ELF sha256, the chain id and layout 1" || fail "vkey.json fields" "$(cat "$OUTD/vkey.json")"
[[ "$(jq -r '.zisk, .scheme' "$OUTD/vkey.json" | tr '\n' ' ')" == "1.3.1-alpha 2 " ]] && jq -e '.scheme | type == "number"' "$OUTD/vkey.json" >/dev/null && pass "vkey.json names the ZisK release (1.3.1-alpha) and its scheme number (2, a number)" || fail "vkey.json zisk and scheme" "$(cat "$OUTD/vkey.json")"
# The two source tags in vkey.json are the ones the image cloned (its files), not the container's environment.
[[ "$(jq -r .source "$OUTD/vkey.json")" == "built by rollup guest-build from rome-zk-evm v0.2.2 and rome-zk-guest v0.3.1" ]] && pass "vkey.json's source line carries the tags the image cloned (v0.2.2, v0.3.1), not the environment's" || fail "vkey.json source" "$(jq -r .source "$OUTD/vkey.json")"

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
# ---- the 1.2.0-alpha image: the same script, the other pin file ---------------------------------------------------------------------
is_release 1.2.0-alpha; cargo_says 1.2.0-alpha "$COMMIT_1_2_0"
mkdir -p "$WORK/keys120/provingKey/zisk/vadcop_final"; echo "$KEY_1_2_0" > "$WORK/keys120/provingKey/zisk/vadcop_final/vadcop_final.verkey.json"
reset; run --expect-chain-id 4295391538 --proving-key-dir "$WORK/keys120/provingKey"
[[ $RC -eq 0 ]] && pass "the 1.2.0-alpha image accepts the 1.2.0-alpha keys" || fail "the 1.2.0-alpha image accepts the 1.2.0-alpha keys" "rc=$RC $ERR"
[[ "$(jq -r '.zisk, .scheme, .rootCVadcopFinal' "$OUTD/vkey.json" | tr '\n' ' ')" == "1.2.0-alpha 1 $ROOT_1_2_0 " ]] && pass "its vkey.json says 1.2.0-alpha, scheme 1, and the 1.2.0-alpha root" || fail "1.2.0-alpha vkey.json" "$(cat "$OUTD/vkey.json")"
reset; run --expect-chain-id 4295391538 --proving-key-dir "$KEYS"; refused ProvingKeyMismatch "the 1.3.1-alpha keys given to the 1.2.0-alpha image"
grep -q '1\.3\.1-alpha' <<<"$ERR" && grep -q '1\.2\.0-alpha' <<<"$ERR" && pass "that refusal names both releases too" || fail "that refusal names both releases" "$ERR"
cargo_says 1.3.1-alpha "$COMMIT_1_3_1"; reset; run --expect-chain-id 4295391538; refused ZiskToolchainMismatch "the 1.3.1-alpha cargo-zisk in the 1.2.0-alpha image"
finish guest_build_image_script
