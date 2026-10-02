#!/usr/bin/env bash
# Runs the cross-repo wire proof, skipping loud (never silently) when the fork checkout it needs is
# absent — a Cargo path dependency cannot be made conditional at the manifest level, so this script is
# the "skip with a clear message" this crate wants; `cargo test` on its own would instead fail
# with a raw "failed to read directory" error if `.fork/` is missing.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
fork_guest="$repo_root/.fork/crates/clients/rome/guest"

if [ ! -d "$fork_guest" ]; then
  echo "SKIPPED: the cross-repo wire proof needs a checkout of rome-protocol/zisk-eth-client (branch" >&2
  echo "rome-guest) at $repo_root/.fork — not found." >&2
  echo "  git clone https://github.com/rome-protocol/zisk-eth-client $repo_root/.fork" >&2
  echo "  cd $repo_root/.fork && git checkout rome-guest" >&2
  echo "  git submodule update --init third_party/ziskethone" >&2
  echo "then rerun this script. .fork/ must never be committed into rome-zk (.gitignore excludes it)." >&2
  exit 0
fi

cd "$here"
exec cargo test "$@"
