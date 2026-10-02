#!/bin/sh
# Fake cargo-zisk that forks a background helper that outlives
# the direct child, then blocks in the foreground past any sane test timeout. Exercises whether
# LocalCargoZisk's timeout path kills the whole process GROUP (killpg) or only the direct pid —
# killing only the direct pid leaves `sleep 3117` running as an orphan after prove() returns.
sleep 3117 &
sleep 3118
