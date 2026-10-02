#!/usr/bin/env bash
# scripts/check-sbpf-v3.sh — refuse, BY NAME, any produced Solana program artifact that is not SBPF v3.
# `cargo build-sbf --arch v3` stamps the ELF header's `e_flags` field with the SBPF version (0 = v0, 1 = v1,
# 2 = v2, 3 = v3 — Anza's own `solana_sbpf::elf::EF_SBPF_V2`-style convention: this one byte is the whole
# on-chain version gate).
# `e_flags` is a little-endian u32 at byte offset 48 of a standard 64-byte ELF64 header (16-byte
# `e_ident` + 2+2+4+8+8+8 = 48 bytes of e_type..e_shoff, then e_flags) — read directly with `dd`/`xxd`
# rather than depending on `readelf`/`objdump` being installed (neither is guaranteed on every runner).
#
# A file that is not a 64-bit BPF ELF is refused first (`NotAnElf`: no `\x7fELF` magic or too short;
# `NotABpfElf`: wrong class or machine), so a stray file can never pass on the strength of four bytes at
# offset 48.
#
# Usage: check-sbpf-v3.sh <path/to/program.so> [more.so ...]
# Exit 0 only if EVERY given file is a v3 BPF ELF (e_flags == 3); otherwise refuses `NotSbpfV3: <file>
# (e_flags=<n>)` and exits 1 — never silently accepts a v0/v1/v2 artifact under a v3 label.
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: check-sbpf-v3.sh <path/to/program.so> [more.so ...]" >&2
  exit 1
fi

status=0
for so in "$@"; do
  if [[ ! -f "$so" ]]; then
    echo "NotSbpfV3: $so does not exist" >&2
    status=1
    continue
  fi
  # The first 52 bytes of the header, as hex pairs, so `od` is the only tool needed (POSIX-present on the
  # self-hosted Linux box, GitHub ubuntu-latest and macOS alike - unlike readelf, which a bare macOS has
  # no Linux-ELF-aware copy of).
  hdr="$(dd if="$so" bs=1 count=52 2>/dev/null | od -An -tx1 -v | tr -d ' \n')"
  # Offsets (bytes): 0-3 magic, 4 class, 18-19 e_machine (little-endian), 48-51 e_flags (little-endian).
  # Refuse anything that is not a 64-bit BPF ELF BEFORE reading e_flags - a non-ELF file whose bytes at
  # offset 48 happen to read 03 00 00 00 must not pass as v3.
  if [[ ${#hdr} -ne 104 || "${hdr:0:8}" != "7f454c46" ]]; then
    echo "NotAnElf: $so (no ELF magic in the first 4 bytes, or shorter than a 52-byte ELF header)" >&2
    status=1
    continue
  fi
  class="${hdr:8:2}"
  machine="${hdr:38:2}${hdr:36:2}"
  # ELFCLASS64 = 02. e_machine 0x00f7 = Linux BPF (what an SBPF v3 build stamps); 0x0107 = SBF (v0 builds).
  if [[ "$class" != "02" || ( "$machine" != "00f7" && "$machine" != "0107" ) ]]; then
    echo "NotABpfElf: $so (ELF class 0x$class, e_machine 0x$machine; want a 64-bit BPF/SBF ELF)" >&2
    status=1
    continue
  fi
  # little-endian: reverse the byte pairs before parsing as hex.
  be_hex="${hdr:102:2}${hdr:100:2}${hdr:98:2}${hdr:96:2}"
  flags=$((16#$be_hex))
  if [[ "$flags" != "3" ]]; then
    echo "NotSbpfV3: $so (e_flags=$flags, want 3)" >&2
    status=1
  else
    echo "sbpf-v3 OK: $so"
  fi
done

exit "$status"
