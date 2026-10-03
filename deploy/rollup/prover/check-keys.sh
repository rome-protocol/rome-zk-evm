#!/usr/bin/env bash
# Checks the ZisK proving keys under $ZISK_HOME against a manifest of pinned hashes. The prover container runs it
# before every start; you run it yourself with --write once, right after downloading the keys, to pin your copy.
#
# Manifest: one line per directory, `<aggregate-sha256>  <directory-name>`. The aggregate hash is
#   (cd <dir> && find . -type f ! -name '*.consttree' -exec sha256sum {} + | LC_ALL=C sort -k2 | sha256sum)
# The *.consttree files are left out: ziskup generates them on the host at install (a GPU install writes them its own
# way), so they are not part of the download and would not match a pin made on another host. With paths relative to the directory, so the host (where you pin) and the container (where the keys sit under
# /opt/zisk) agree whatever ZISK_HOME is. The sort is bytewise (LC_ALL=C, which this script sets itself): with another
# locale, such as en_US.UTF-8 on many hosts, sort ignores case and punctuation and orders the same names differently, so
# a pin made on the host would not match the container. An added, removed, renamed or changed file shows up as a mismatch. The placeholder UNPINNED_PENDING_FIRST_BOOTSTRAP is
# never a match: a plain run refuses it by name, and only --write replaces it (by hashing the real directories).
#
# Exit codes name the reason:
#   0  OK                   both directories are populated and match the manifest
#   10 KeysNotPopulated     a directory is missing or empty
#   11 KeysManifestUnpinned the manifest still has the placeholder
#   12 KeysShaMismatch      a hash differs, or the manifest has no entry for a directory
#   13 KeysManifestMissing  the manifest file does not exist
# When several apply, NotPopulated wins over Mismatch, which wins over Unpinned.
set -euo pipefail
export LC_ALL=C   # sort and find order bytewise, so the host and the container hash the same lines in the same order

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZISK_HOME="${ZISK_HOME:?ZISK_HOME must be set (the directory the proving keys were downloaded to)}"
MANIFEST="${MANIFEST:-$SCRIPT_DIR/keys.sha256}"
DIRS=(provingKey provingKeySnark)

WRITE=0
if [[ "${1:-}" == "--write" ]]; then WRITE=1
elif [[ -n "${1:-}" ]]; then echo "usage: check-keys.sh [--write]" >&2; exit 1; fi

# The container has sha256sum; macOS has shasum. An array, because find -exec runs a real binary.
if command -v sha256sum >/dev/null 2>&1; then SHA256_CMD=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then SHA256_CMD=(shasum -a 256)
else echo "NoSha256Tool: neither sha256sum nor shasum is on PATH" >&2; exit 127; fi

agg_hash() { # $1 = directory -> its aggregate sha256, or nothing when it is missing or empty
  local dir="$1"
  if [[ ! -d "$dir" ]] || [[ -z "$(find "$dir" -type f -print -quit 2>/dev/null)" ]]; then echo ""; return 0; fi
  (cd "$dir" && find . -type f ! -name '*.consttree' -exec "${SHA256_CMD[@]}" {} + 2>/dev/null | sort -k2 | "${SHA256_CMD[@]}" | awk '{print $1}')
}
manifest_value() { grep -E "  ${1}\$" "$MANIFEST" 2>/dev/null | tail -1 | awk '{print $1}'; }

if [[ "$WRITE" -eq 1 ]]; then
  tmp="$(mktemp)"
  {
    echo "# Pinned aggregate sha256 of the ZisK proving keys, written by check-keys.sh --write (see check-keys.sh for the recipe)."
    echo "# The *.consttree files are generated on the host at install and are left out of the hash."
    for d in "${DIRS[@]}"; do
      h="$(agg_hash "$ZISK_HOME/$d")"
      if [[ -z "$h" ]]; then echo "refusing to write a manifest entry for '$d': $ZISK_HOME/$d is missing or empty" >&2; rm -f "$tmp"; exit 1; fi
      printf '%s  %s\n' "$h" "$d"
    done
  } > "$tmp"
  cat "$tmp" > "$MANIFEST"; rm -f "$tmp"
  echo "wrote $MANIFEST"
  exit 0
fi

if [[ ! -f "$MANIFEST" ]]; then
  echo "KeysManifestMissing: $MANIFEST does not exist" >&2
  exit 13
fi

NOT_POPULATED=0; MISMATCH=0; UNPINNED=0
for d in "${DIRS[@]}"; do
  got="$(agg_hash "$ZISK_HOME/$d")"
  if [[ -z "$got" ]]; then echo "KeysNotPopulated: $ZISK_HOME/$d is missing or empty" >&2; NOT_POPULATED=1; continue; fi
  pinned="$(manifest_value "$d")"
  if [[ -z "$pinned" ]]; then echo "KeysShaMismatch: $MANIFEST names no entry for '$d'" >&2; MISMATCH=1; continue; fi
  if [[ "$pinned" == "UNPINNED_PENDING_FIRST_BOOTSTRAP" ]]; then
    echo "KeysManifestUnpinned: $MANIFEST still has the placeholder for '$d'; pin your downloaded keys with: ZISK_HOME=$ZISK_HOME MANIFEST=$MANIFEST bash check-keys.sh --write" >&2
    UNPINNED=1; continue
  fi
  if [[ "$got" != "$pinned" ]]; then echo "KeysShaMismatch: $d hashes to $got, $MANIFEST pins $pinned; refusing to start" >&2; MISMATCH=1; continue; fi
  echo "OK: $d matches $MANIFEST ($pinned)"
done

if [[ "$NOT_POPULATED" -eq 1 ]]; then exit 10
elif [[ "$MISMATCH" -eq 1 ]]; then exit 12
elif [[ "$UNPINNED" -eq 1 ]]; then exit 11; fi
exit 0
