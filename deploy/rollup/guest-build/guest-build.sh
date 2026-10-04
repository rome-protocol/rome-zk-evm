#!/usr/bin/env bash
# Runs inside the guest-build image: builds the batch guest for one genesis and writes the ELF and vkey.json.
#
#   guest-build --expect-chain-id N [--proving-key-dir DIR]
#
# Reads /in/genesis.json (mounted read-only). Writes /out/<elf sha256>.elf and /out/vkey.json. With a ZisK proving key
# directory it also computes the programVK (cargo-zisk setup -e: no proof, about 40 seconds, about 36 GB of memory);
# without one, or on a machine with too little memory, vkey.json is written without programVK and the output says so.
# Every refusal starts with a CamelCase name on stderr and exits non-zero.
set -uo pipefail

# The three locations are the image's own; the tests point them at a scratch directory.
IN_DIR="${GUEST_BUILD_IN:-/in}"
GENESIS="$IN_DIR/genesis.json"
OUT="${GUEST_BUILD_OUT:-/out}"
WORK_DIR="${GUEST_BUILD_WORK:-/work}"
MEMINFO="${GUEST_BUILD_MEMINFO:-/proc/meminfo}"
GUEST_DIR="$WORK_DIR/rome-zk-evm/.fork/bin/guests/stateless-validator-rome"
# The root of the vadcop_final verification key in the ZisK 1.2.0-alpha key set, the four 64-bit limbs of
# vadcop_final.verkey.json written as one 32-byte big-endian number. It is a property of the key set, the same for every
# guest. With the keys mounted it is read from them and must match.
ROOT_C_VADCOP_FINAL=0x564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f
# cargo-zisk setup measured 36.6 GB resident for a guest of this size. Below this much memory it is not tried.
MIN_MEM_KIB=41943040   # 40 GiB

die() { echo "$1: $2" >&2; exit 1; }

EXPECT=""
KEYS=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --expect-chain-id) [[ $# -ge 2 ]] || die FlagValueMissing "--expect-chain-id needs a number"; EXPECT="$2"; shift 2 ;;
    --proving-key-dir) [[ $# -ge 2 ]] || die FlagValueMissing "--proving-key-dir needs a path"; KEYS="$2"; shift 2 ;;
    *) die UnknownArgument "$1" ;;
  esac
done
[[ "$EXPECT" =~ ^[0-9]+$ ]] || die ChainIdMissing "--expect-chain-id must be the chain's id, a whole number (got '${EXPECT}')"
[[ -f "$GENESIS" ]] || die GenesisMissing "$GENESIS is not mounted"
[[ -d "$OUT" && -w "$OUT" ]] || die OutputDirNotWritable "$OUT is not a writable mount"

# The key directory is checked before the build, which takes minutes: keys of another release would give a programVK that no
# verifier accepts.
if [[ -n "$KEYS" ]]; then
  [[ -d "$KEYS" ]] || die ProvingKeyDirMissing "$KEYS is not mounted"
  vk_json="$KEYS/zisk/vadcop_final/vadcop_final.verkey.json"
  [[ -f "$vk_json" ]] || die ProvingKeyInvalid "$vk_json is not there; $KEYS is not a ZisK 1.2.0-alpha proving key directory"
  root_c="$(python3 -c 'import json,sys; l=json.load(open(sys.argv[1])); assert len(l)==4; print("0x"+"".join("%016x"%int(x) for x in l))' "$vk_json" 2>/dev/null)" \
    || die ProvingKeyInvalid "$vk_json is not a verification key file"
  [[ "$root_c" == "$ROOT_C_VADCOP_FINAL" ]] || die ProvingKeyMismatch "$KEYS has vadcop_final root $root_c, but the ZisK 1.2.0-alpha key set has $ROOT_C_VADCOP_FINAL"
fi

echo "guest-build: rome-zk-evm $(cat "$WORK_DIR/rome-zk-evm.rev") (${ROME_ZK_EVM_TAG:-unknown}), rome-zk-guest $(cat "$WORK_DIR/rome-zk-guest.rev") (${ROME_ZK_GUEST_TAG:-unknown})" >&2
LOG="$(mktemp)"
"$GUEST_DIR/build-elf.sh" --genesis "$GENESIS" --expect-chain-id "$EXPECT" 2>&1 >"$LOG.out" | tee "$LOG" >&2
rc=${PIPESTATUS[0]}
if [[ $rc -ne 0 ]]; then
  # The build script and the guest's build.rs name what they refuse; carry that name out.
  line="$(grep -E 'REFUSING|GenesisFundedAccountLimit|GenesisBalanceUnreadable|GenesisAllocMalformed' "$LOG" | head -n 1)"
  case "$line" in
    *GenesisFundedAccountLimit*|*GenesisBalanceUnreadable*|*GenesisAllocMalformed*) name="$(grep -oE 'Genesis(FundedAccountLimit|BalanceUnreadable|AllocMalformed)' <<<"$line" | head -n 1)" ;;
    *"chain id mismatch"*) name=ChainIdMismatch ;;
    *) name=GuestBuildFailed ;;
  esac
  die "$name" "the guest build stopped (exit $rc)${line:+: ${line#*REFUSING — }}"
fi

val() { sed -n "s/^build-elf.sh: $1=//p" "$LOG.out" "$LOG" | head -n 1; }
SHA="$(val sha256)"; GSHA="$(val genesis_sha256)"; CID="$(val chain_id)"; BAL="$(val genesis_balance)"
[[ "$SHA" =~ ^[0-9a-f]{64}$ ]] || die GuestBuildFailed "the build script printed no ELF sha256"
[[ "$CID" == "$EXPECT" ]] || die ChainIdMismatch "the built guest embeds chain id '$CID', expected $EXPECT"
ELF_SRC="$(sed -n 's/^build-elf.sh: built //p' "$LOG.out" "$LOG" | head -n 1)"
[[ -f "$ELF_SRC" ]] || die GuestBuildFailed "the built ELF '$ELF_SRC' is missing"
cp "$ELF_SRC" "$OUT/$SHA.elf"
[[ "$(sha256sum "$OUT/$SHA.elf" | cut -d' ' -f1)" == "$SHA" ]] || die GuestBuildFailed "the copied ELF does not hash to $SHA"

# --- the programVK ------------------------------------------------------------------------------------------------
PROGRAM_VK=""
SKIP=""
if [[ -z "$KEYS" ]]; then
  SKIP="ProgramVkNotComputed: no ZisK proving key directory was given, so vkey.json has no programVK. Run this on a machine with the keys (ZISK_HOME) and at least 40 GiB of memory, or get the programVK from Rome."
else
  mem_kib="$(awk '/^MemTotal:/ {print $2}' "$MEMINFO")"
  if [[ -z "$mem_kib" || "$mem_kib" -lt "$MIN_MEM_KIB" ]]; then
    SKIP="ProgramVkNotComputed: this machine has $(( ${mem_kib:-0} / 1048576 )) GiB of memory and the programVK step needs about 36 GB (40 GiB to be safe), so vkey.json has no programVK. Run it on a larger machine, or get the programVK from Rome."
  else
    echo "guest-build: computing the programVK (cargo-zisk setup -e; no proof)" >&2
    SETUP_LOG="$(mktemp)"
    if ! cargo-zisk setup -e "$OUT/$SHA.elf" -k "$KEYS" > "$SETUP_LOG" 2>&1; then
      tail -n 20 "$SETUP_LOG" >&2
      die ProgramVkFailed "cargo-zisk setup -e failed (exit above); the ELF is at $OUT/$SHA.elf"
    fi
    PROGRAM_VK="$(sed -n 's/.*Root hash: \[\(.*\)\].*/\1/p' "$SETUP_LOG" | python3 -c 'import sys
s=sys.stdin.read().strip()
l=[int(x) for x in s.split(",")] if s else []
print("0x"+"".join("%016x"%x for x in l) if len(l)==4 else "")')"
    [[ "$PROGRAM_VK" =~ ^0x[0-9a-f]{64}$ ]] || { tail -n 20 "$SETUP_LOG" >&2; die ProgramVkFailed "cargo-zisk setup -e printed no root hash"; }
  fi
fi

# --- vkey.json: the format rome-zk-prover reads (crates/rome-zk-prover/src/config.rs) ----------------------------------
python3 - "$OUT/vkey.json" "$PROGRAM_VK" "$ROOT_C_VADCOP_FINAL" "$SHA" "$CID" "$GSHA" "${ROME_ZK_EVM_TAG:-unknown}" "${ROME_ZK_GUEST_TAG:-unknown}" <<'PY' || die VkeyWriteFailed "could not write $OUT/vkey.json"
import json, sys
path, pvk, root_c, sha, cid, gsha, evm_tag, guest_tag = sys.argv[1:]
d = {}
if pvk:
    d["programVK"] = pvk
d.update({
    "rootCVadcopFinal": root_c,
    "elf_sha256": sha,
    "chain_id": int(cid),
    "layout_id": 1,
    "zisk": "1.2.0-alpha",
    "genesis_sha256": gsha,
    "source": "built by rollup guest-build from rome-zk-evm %s and rome-zk-guest %s" % (evm_tag, guest_tag),
})
with open(path, "w") as f:
    json.dump(d, f, indent=2)
    f.write("\n")
PY

# Hand the files to whoever started the container, so they are theirs on the host.
if [[ -n "${OUT_UID:-}" && -n "${OUT_GID:-}" ]]; then chown "$OUT_UID:$OUT_GID" "$OUT/$SHA.elf" "$OUT/vkey.json" 2>/dev/null || true; fi

echo "guest_elf=$SHA.elf"
echo "elf_sha256=$SHA"
echo "genesis_sha256=$GSHA"
echo "chain_id=$CID"
echo "genesis_balance=$BAL"
echo "program_vk=${PROGRAM_VK:-none}"
[[ -z "$SKIP" ]] || echo "$SKIP" >&2
exit 0
