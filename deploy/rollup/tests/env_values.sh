#!/usr/bin/env bash
# deploy/rollup/tests/env_values.sh — .env values are read the way an .env file is normally written: surrounding
# quotes are removed, an inline ` # comment` is dropped, and a value that was quoted never reaches a rendered TOML
# with its quotes still on (that would pass init and fail only when the services start).
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/fixtures/common.sh"
source "$ROLLUP_DIR/lib/config-lib.sh"
E="$WORK/test.env"
cat > "$E" <<'ENV'
PLAIN=https://rpc.example.invalid/path?key=abc#frag
DQ="https://rpc.example.invalid"
SQ='https://rpc.example.invalid'
COMMENT=https://rpc.example.invalid   # my provider
QUOTED_COMMENT="https://rpc.example.invalid" # note
INNER_HASH="abc # not a comment"
EMPTY=
SPACED=  value with spaces  
TWICE=first
TWICE=second
EQ=a=b=c
ENV
printf 'CRLF=value\r\n' >> "$E"
check() { # $1 key $2 expected
  local got; got="$(rz_env_get "$E" "$1")"
  [[ "$got" == "$2" ]] && pass "rz_env_get $1 -> '$2'" || fail "rz_env_get $1" "got '$got', wanted '$2'"
}
check PLAIN 'https://rpc.example.invalid/path?key=abc#frag'
check DQ 'https://rpc.example.invalid'
check SQ 'https://rpc.example.invalid'
check COMMENT 'https://rpc.example.invalid'
check QUOTED_COMMENT 'https://rpc.example.invalid'
check INNER_HASH 'abc # not a comment'
check EMPTY ''
check SPACED 'value with spaces'
check TWICE second
check EQ 'a=b=c'
check CRLF value
check MISSING ''

# End to end: a quoted SOLANA_RPC_URL renders a valid TOML.
setup_fixture
sed -i.bak 's|^SOLANA_RPC_URL=.*|SOLANA_RPC_URL="https://rpc.example.invalid"  # quoted, with a comment|' "$WORK/rollup.env"
if out="$("$ROLLUP" init 2>&1)"; then
  python3 - "$ROLLUP_OUT/batcher.toml" "$ROLLUP_OUT/derive.toml" <<'PY' && pass "quoted SOLANA_RPC_URL renders valid batcher.toml and derive.toml with the bare URL" || fail "quoted SOLANA_RPC_URL renders valid TOML" "see above"
import sys, tomllib
for f in sys.argv[1:]:
    d = tomllib.load(open(f, "rb"))
    urls = [v for k, v in d.items() if k in ("rpc_url", "solana_rpc_url")]
    assert urls == ["https://rpc.example.invalid"], (f, urls)
PY
else fail "init with a quoted SOLANA_RPC_URL" "$out"; fi
finish env_values
