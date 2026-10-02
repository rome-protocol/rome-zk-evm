#!/bin/sh
# Fake cargo-zisk that never returns within any sane test timeout, exercising LocalCargoZisk's own
# timeout + kill path.
sleep 5
exit 0
