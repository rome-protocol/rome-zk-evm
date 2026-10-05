#!/bin/sh
# Fake cargo-zisk that never returns within any sane test timeout, exercising LocalCargoZisk's own
# timeout + kill path.
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
sleep 5
exit 0
