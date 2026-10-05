#!/usr/bin/env bash
# Run by the guest-build image right after it clones the guest:
#
#   check-guest-lock RELEASE CARGO_LOCK
#
# The guest is built with the ZisK toolchain of the image, but the ZisK crates it links (ziskos and the other zisk-* crates)
# come from its own Cargo.lock. A guest locked to the crates of one release and built with the toolchain of another gives an
# ELF that belongs to neither, so the image stops unless every one of those crates is at the image's release. Every refusal
# starts with a CamelCase name on stderr and exits non-zero.
set -uo pipefail
die() { echo "$1: $2" >&2; exit 1; }

RELEASE="${1:-}"; LOCK="${2:-}"
[[ -n "$RELEASE" ]] || die ZiskReleaseUnknown "no ZisK release was given to check the guest against"
[[ -f "$LOCK" ]] || die GuestLockMissing "$LOCK is not there: the guest has no Cargo.lock to read its ZisK crates from"

# One "name version" line per package of the lock file, for ziskos and every crate whose name starts with zisk-.
CRATES="$(awk '
  /^name = "/    { gsub(/^name = "|"$/, ""); name = $0; next }
  /^version = "/ { gsub(/^version = "|"$/, ""); if (name == "ziskos" || name ~ /^zisk-/) print name, $0; name = ""; next }
  /^\[\[package\]\]/ { name = "" }
' "$LOCK")"

grep -q '^ziskos ' <<<"$CRATES" || die GuestZiskosMissing "$LOCK pins no ziskos crate, so it is not a guest this image can tell the ZisK release of"
BAD="$(awk -v r="$RELEASE" '$2 != r { printf "%s%s %s", sep, $1, $2; sep = ", " }' <<<"$CRATES")"
[[ -z "$BAD" ]] || die GuestZiskMismatch "the guest's Cargo.lock pins ZisK crates that are not release $RELEASE (${BAD}), but this image builds with the ZisK $RELEASE toolchain; use a guest tag locked to $RELEASE, or an image of the release the guest is locked to"
echo "check-guest-lock: ziskos and every zisk-* crate in the guest's Cargo.lock are at $RELEASE" >&2
