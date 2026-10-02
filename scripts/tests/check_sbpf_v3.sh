#!/usr/bin/env bash
# scripts/tests/check_sbpf_v3.sh — proves scripts/check-sbpf-v3.sh accepts only a real SBPF v3 ELF and
# refuses everything else by name:
#   - v3 (e_flags 3, BPF machine)                        -> "sbpf-v3 OK", exit 0
#   - v0 / v1 / v2 (e_flags 0 / 1 / 2)                    -> NotSbpfV3
#   - a file that is not an ELF at all, even one whose bytes at offset 48 read 03 00 00 00
#     (a 52-byte file of zeros plus that word)            -> NotAnElf
#   - a truncated header, an empty file                   -> NotAnElf
#   - a 32-bit ELF, or an ELF for another machine, that carries e_flags 3 -> NotABpfElf
#   - a missing file                                      -> NotSbpfV3
#   - a mixed list refuses (exit 1) and still names every file
# The headers are built byte by byte here; no real program has to be compiled.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="${CHECK_CMD:-$SCRIPT_DIR/../check-sbpf-v3.sh}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

# elf_header <file> <class byte> <machine low> <machine high> <flags byte 0>
# A 64-byte ELF64 little-endian header: magic, class, data=1, version=1, e_type=3, machine, e_version=1,
# e_flags at offset 48.
elf_header() {
  local f="$1" class="$2" mlo="$3" mhi="$4" flags="$5"
  {
    printf '\x7f\x45\x4c\x46'            # 0-3   magic
    printf "\\x$class"                    # 4     class
    printf '\x01\x01'                     # 5-6   little-endian, version 1
    printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00'   # 7-15
    printf '\x03\x00'                     # 16-17 e_type
    printf "\\x$mlo\\x$mhi"               # 18-19 e_machine
    printf '\x01\x00\x00\x00'             # 20-23 e_version
    printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00'  # 24-47
    printf "\\x$flags\\x00\\x00\\x00"     # 48-51 e_flags
    printf '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00'  # 52-63
  } > "$f"
}

# expect <label> <expected exit> <text that must appear> <file...>
expect() {
  local label="$1" want="$2" text="$3" out code
  shift 3
  out="$("$CHECK" "$@" 2>&1)"; code=$?
  if [[ $code -eq $want ]] && grep -q -- "$text" <<<"$out"; then
    pass "$label"
  else
    fail "$label" "exit=$code (want $want) out=$out"
  fi
}

# BPF machine: 0xf7 (Linux BPF, what a v3 build stamps). SBF v0 stamps 0x107.
elf_header "$WORK/v3.so" 02 f7 00 03
elf_header "$WORK/v0.so" 02 07 01 00
elf_header "$WORK/v1.so" 02 07 01 01
elf_header "$WORK/v2.so" 02 07 01 02
elf_header "$WORK/elf32.so" 01 f7 00 03
elf_header "$WORK/x86.so" 02 3e 00 03

# A fake: 48 zero bytes, then 03 00 00 00 — 52 bytes, not an ELF.
{ head -c 48 /dev/zero; printf '\x03\x00\x00\x00'; } > "$WORK/fake.so"
# A truncated header (valid magic, cut short) and an empty file.
head -c 20 "$WORK/v3.so" > "$WORK/short.so"
: > "$WORK/empty.so"

expect "v3 ELF is accepted" 0 "sbpf-v3 OK: $WORK/v3.so" "$WORK/v3.so"
expect "v0 ELF is refused" 1 "NotSbpfV3: $WORK/v0.so (e_flags=0, want 3)" "$WORK/v0.so"
expect "v1 ELF is refused" 1 "NotSbpfV3: $WORK/v1.so (e_flags=1, want 3)" "$WORK/v1.so"
expect "v2 ELF is refused" 1 "NotSbpfV3: $WORK/v2.so (e_flags=2, want 3)" "$WORK/v2.so"
expect "52-byte non-ELF with 03 00 00 00 at offset 48 is refused" 1 "NotAnElf: $WORK/fake.so" "$WORK/fake.so"
expect "truncated ELF header is refused" 1 "NotAnElf: $WORK/short.so" "$WORK/short.so"
expect "empty file is refused" 1 "NotAnElf: $WORK/empty.so" "$WORK/empty.so"
expect "32-bit ELF with e_flags 3 is refused" 1 "NotABpfElf: $WORK/elf32.so" "$WORK/elf32.so"
expect "ELF for another machine with e_flags 3 is refused" 1 "NotABpfElf: $WORK/x86.so" "$WORK/x86.so"
expect "missing file is refused" 1 "NotSbpfV3: $WORK/nope.so does not exist" "$WORK/nope.so"
expect "a mixed list is refused" 1 "NotSbpfV3: $WORK/v0.so" "$WORK/v3.so" "$WORK/v0.so"
expect "a mixed list still names the good file" 1 "sbpf-v3 OK: $WORK/v3.so" "$WORK/v3.so" "$WORK/v0.so"

echo "== check_sbpf_v3.sh :: $( [[ $FAILED -eq 0 ]] && echo ALL PASS || echo SOME FAILED ) =="
exit "$FAILED"
