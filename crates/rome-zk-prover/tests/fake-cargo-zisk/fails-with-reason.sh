#!/bin/sh
# Fake cargo-zisk that exits non-zero after naming its reason on stderr, the way the real tool names a
# missing GPU constant file.
if [ "${1:-}" = "--version" ]; then
  echo "cargo-zisk 1.3.1-alpha [cpu] (306a9c9 fake-build)"
  exit 0
fi
echo "starting proof" >&2
echo "Error: cannot open constants file /fake/home/provingKey/zisk/vadcop_final/vadcop_final.consttree.gpu: No such file or directory" >&2
exit 1
