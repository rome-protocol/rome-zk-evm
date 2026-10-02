#!/bin/sh
# A helper that escapes into its own new session/process group (setsid) while still inheriting
# the piped stdout/stderr file descriptors. `killpg` on the direct child's own process group can
# never reach a helper that has done this -- only a BOUNDED drain (never an unconditional join)
# keeps `prove()` itself from hanging on it. `prove()` returning promptly here does not claim the
# escaped helper is also killed (it is not, by construction of this test); that is the accepted
# bound of the process-group defense, not a claim this crate makes about every possible tool.
setsid sh -c 'sleep 3119' &
sleep 3120
