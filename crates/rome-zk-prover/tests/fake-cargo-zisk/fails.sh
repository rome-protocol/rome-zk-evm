#!/bin/sh
# Fake cargo-zisk that exits non-zero, as if the STARK/PLONK proving step itself failed.
echo "cargo-zisk: proving failed" >&2
exit 1
