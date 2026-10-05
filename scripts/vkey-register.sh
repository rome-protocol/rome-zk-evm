#!/usr/bin/env bash
# scripts/vkey-register.sh — Rome's checked registration of a chain's verification key.
#
#   vkey-register.sh --settlement P --chain-id N --genesis FILE --zisk RELEASE --guest-tag TAG --elf-sha256 HEX --program-vk HEX \
#       --registry-keypair FILE --payer-keypair FILE (--activation-slot N | --activation-delay-slots N) \
#       [--evm-tag TAG] [--anchor-guest-tag TAG] [--anchor-zisk RELEASE] [--anchor-evm-tag TAG] [--bridge-program ID] [--move-pending-activation] \
#       [--proving-key-dir DIR] [--anchor-proving-key-dir DIR] [--rpc-url URL] [--docker "sudo docker"] [--confirm]
#   vkey-register.sh show --settlement P --chain-id N [--rpc-url URL]
#
# The first form is `rome-zk-ops vkey register`: it reads the chain's root on the settlement program, rebuilds the guest
# from the operator's genesis.json in the pinned guest-build image (deploy/rollup/guest-build, the image
# `./rollup guest-build` uses), and sends SetRegistryEntry only if the chain id, the genesis, the ELF's sha256 and the
# programVK all match what the operator sent, the genesis alloc is the one the rollup template renders, and, when the chain
# has a declared balance, that Rome's vault for it holds it. Once the chain's root has moved past genesis, --anchor-guest-tag TAG is required: the genesis is rebuilt at that tag
# and must reproduce a registered key. Without --confirm it prints the instruction and the slot it would set and
# sends nothing. --zisk names the ZisK release the key is for (for example 1.3.1-alpha); the guest is rebuilt in that release's
# image. The programVK is recomputed from the rebuild, so the proving keys of that release are needed: --proving-key-dir, or
# ZISK_HOME (its provingKey directory). Each release has its own keys and the image refuses another release's, so an anchor
# in another release (--anchor-zisk) also needs that release's directory, --anchor-proving-key-dir, when the chain has moved
# past genesis.
#
# The binary is $ROME_ZK_OPS when set, else it is built and run from this checkout. Keys are read from the files named and
# never printed. Every transaction goes out as a V1 transaction through rome-zk-solana-sender.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$1: $2" >&2; exit 2; }

ops() {
  if [[ -n "${ROME_ZK_OPS:-}" ]]; then "$ROME_ZK_OPS" "$@"
  else (cd "$ROOT" && cargo run --quiet --release -p rome-zk-ops -- "$@"); fi
}

sub=register
if [[ "${1:-}" == show ]]; then sub=show; shift; fi

declare -A V=()
CONFIRM=0
MOVE_PENDING=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --confirm) CONFIRM=1; shift ;;
    --move-pending-activation) MOVE_PENDING=1; shift ;;
    --settlement|--chain-id|--genesis|--guest-tag|--elf-sha256|--program-vk|--registry-keypair|--payer-keypair|--activation-slot|--activation-delay-slots|--evm-tag|--anchor-guest-tag|--anchor-evm-tag|--bridge-program|--proving-key-dir|--anchor-proving-key-dir|--anchor-zisk|--zisk|--rpc-url|--docker)
      [[ $# -ge 2 ]] || die FlagValueMissing "$1 needs a value"
      V["$1"]="$2"; shift 2 ;;
    *) die UnknownArgument "$1 is not a flag of this command" ;;
  esac
done

need() { [[ -n "${V[$1]:-}" ]] || die "${2}Missing" "$1 is required"; }
global=()
[[ -z "${V[--rpc-url]:-}" ]] || global+=(--rpc-url "${V[--rpc-url]}")

if [[ "$sub" == show ]]; then
  need --settlement Settlement; need --chain-id ChainId
  [[ $CONFIRM -eq 0 ]] || die UnknownArgument "--confirm sends nothing here: show only reads"
  for k in "${!V[@]}"; do
    case "$k" in --settlement|--chain-id|--rpc-url) ;; *) die UnknownArgument "$k is not a flag of 'show'" ;; esac
  done
  ops "${global[@]}" vkey show --settlement "${V[--settlement]}" --chain-id "${V[--chain-id]}"
  exit $?
fi

need --settlement Settlement; need --chain-id ChainId; need --genesis Genesis; need --zisk Zisk; need --guest-tag GuestTag
need --elf-sha256 ElfSha256; need --program-vk ProgramVk; need --registry-keypair RegistryKeypair; need --payer-keypair PayerKeypair
[[ -f "${V[--genesis]}" ]] || die GenesisUnreadable "${V[--genesis]} is not a file"
if [[ -n "${V[--activation-slot]:-}" && -n "${V[--activation-delay-slots]:-}" ]]; then
  die ActivationConflict "give --activation-slot or --activation-delay-slots, not both"
fi
[[ -n "${V[--activation-slot]:-}${V[--activation-delay-slots]:-}" ]] || die ActivationMissing "give --activation-slot N or --activation-delay-slots N: the slot the key becomes usable has no default"

keys="${V[--proving-key-dir]:-}"
if [[ -z "$keys" && -n "${ZISK_HOME:-}" ]]; then keys="$ZISK_HOME/provingKey"; fi
[[ -n "$keys" ]] || die ProvingKeyDirMissing "give --proving-key-dir, or set ZISK_HOME: the programVK is recomputed from the rebuild"
[[ -d "$keys" ]] || die ProvingKeyDirMissing "$keys is not a directory"
if [[ -n "${V[--anchor-proving-key-dir]:-}" && ! -d "${V[--anchor-proving-key-dir]}" ]]; then
  die ProvingKeyDirMissing "${V[--anchor-proving-key-dir]} is not a directory"
fi

args=(vkey register
  --settlement "${V[--settlement]}" --chain-id "${V[--chain-id]}" --genesis "${V[--genesis]}" --zisk "${V[--zisk]}"
  --guest-tag "${V[--guest-tag]}" --elf-sha256 "${V[--elf-sha256]}" --program-vk "${V[--program-vk]}"
  --registry-keypair "${V[--registry-keypair]}" --payer-keypair "${V[--payer-keypair]}"
  --proving-key-dir "$keys" --guest-build-dir "$ROOT/deploy/rollup/guest-build")
[[ -z "${V[--activation-slot]:-}" ]] || args+=(--activation-slot "${V[--activation-slot]}")
[[ -z "${V[--activation-delay-slots]:-}" ]] || args+=(--activation-delay-slots "${V[--activation-delay-slots]}")
[[ -z "${V[--evm-tag]:-}" ]] || args+=(--evm-tag "${V[--evm-tag]}")
[[ -z "${V[--anchor-evm-tag]:-}" ]] || args+=(--anchor-evm-tag "${V[--anchor-evm-tag]}")
[[ -z "${V[--bridge-program]:-}" ]] || args+=(--bridge-program "${V[--bridge-program]}")
[[ -z "${V[--docker]:-}" ]] || args+=(--docker "${V[--docker]}")
[[ -z "${V[--anchor-guest-tag]:-}" ]] || args+=(--anchor-guest-tag "${V[--anchor-guest-tag]}")
[[ -z "${V[--anchor-zisk]:-}" ]] || args+=(--anchor-zisk "${V[--anchor-zisk]}")
[[ -z "${V[--anchor-proving-key-dir]:-}" ]] || args+=(--anchor-proving-key-dir "${V[--anchor-proving-key-dir]}")
[[ $MOVE_PENDING -eq 0 ]] || args+=(--move-pending-activation)
[[ $CONFIRM -eq 0 ]] || global+=(--confirm)

ops "${global[@]}" "${args[@]}"
