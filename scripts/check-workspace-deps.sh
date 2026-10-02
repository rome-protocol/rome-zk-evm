#!/usr/bin/env bash
# scripts/check-workspace-deps.sh — every manifest takes its solana-*, spl-* and agave-* crates from the root
# [workspace.dependencies], and the crates outside the workspace stay on the same versions. The work is in
# check_workspace_deps.py, which reads each tracked Cargo.toml as TOML so it sees the inline, table and
# renamed-import forms alike. Needs python3 >= 3.11 (tomllib), or the tomli package on an older one.
set -euo pipefail
exec python3 "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check_workspace_deps.py" "$@"
