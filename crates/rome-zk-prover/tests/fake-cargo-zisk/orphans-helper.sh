#!/bin/sh
# Fake cargo-zisk that forks a background helper that outlives
# the direct child, then blocks in the foreground past any sane test timeout. Exercises whether
# LocalCargoZisk's timeout path kills the whole process GROUP (killpg) or only the direct pid —
# killing only the direct pid leaves `sleep 3117` running as an orphan after prove() returns.
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
sleep 3117 &
sleep 3118
