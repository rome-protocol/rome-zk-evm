#!/usr/bin/env bash
# deploy/rollup/lib/config-lib.sh — the config renderers, as functions. Sourced, never executed.
#
# One copy of the rendering code: the `rollup` CLI uses it, and so do the in-repo reference scripts that render the
# same configs for Rome's own development chain. Nothing here touches a network or a cloud service: it reads local
# files, writes local files, and calls openssl, python3 and jq for the odd job.
#
# Every function that refuses does so with a CamelCase name at the start of its message (so a test, a log search or an
# operator can find it), writes that message to stderr and returns non-zero.

# rz_env_get FILE KEY -> the last KEY=value line's value, or nothing. Does not source the file (it may hold spaces
# or shell syntax that is not ours to run).
#
# The value is read the way an .env file is normally written: one pair of matching surrounding quotes is removed
# (whatever follows the closing quote is ignored), and for an unquoted value a trailing ` # comment` is dropped along
# with the spaces before it. A `#` with no space before it stays (it can be part of a URL fragment).
rz_env_get() {
  local line v
  line="$(grep -E "^$2=" "$1" 2>/dev/null | tail -1)" || true
  [[ -n "$line" ]] || return 0
  v="${line#*=}"
  v="${v%$'\r'}"; v="${v#"${v%%[![:space:]]*}"}"
  if [[ "$v" =~ ^\"([^\"]*)\" ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  elif [[ "$v" =~ ^\'([^\']*)\' ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  else
    sed -E 's/[[:space:]]+#.*$//; s/[[:space:]]+$//' <<<"$v"
  fi
}

# rz_toml_get FILE KEY... -> for each dotted KEY (for example `profile.blocks_per_batch`) print its value, one per line,
# strings unquoted. A key that is absent prints an empty line. Needs python3 >= 3.11 (tomllib).
rz_toml_get() {
  local file="$1"; shift
  python3 - "$file" "$@" <<'PY'
import sys
try:
    import tomllib
except ModuleNotFoundError:
    sys.exit("Python311Required: python3 3.11 or newer is needed to read chain.toml (found %s)" % sys.version.split()[0])
with open(sys.argv[1], "rb") as f:
    data = tomllib.load(f)
for dotted in sys.argv[2:]:
    cur = data
    for part in dotted.split("."):
        cur = cur.get(part) if isinstance(cur, dict) else None
    print("" if cur is None or isinstance(cur, dict) else cur)
PY
}

# rz_toml_table FILE TABLE -> `key=value` lines for every scalar in [TABLE] (integers and strings).
rz_toml_table() {
  python3 - "$1" "$2" <<'PY'
import sys
try:
    import tomllib
except ModuleNotFoundError:
    sys.exit("Python311Required: python3 3.11 or newer is needed to read chain.toml (found %s)" % sys.version.split()[0])
with open(sys.argv[1], "rb") as f:
    data = tomllib.load(f)
for k, v in (data.get(sys.argv[2]) or {}).items():
    if not isinstance(v, (dict, list)):
        print(f"{k}={v}")
PY
}

# rz_sed_escape VALUE -> VALUE safe as the replacement half of `s|...|VALUE|`; refuses a newline.
rz_sed_escape() {
  local v="$1"
  [[ "$v" != *$'\n'* ]] || return 1
  v="${v//\\/\\\\}"; v="${v//&/\\&}"; v="${v//|/\\|}"
  printf '%s' "$v"
}

# rz_render_template TEMPLATE OUT NAME=value... -> OUT is TEMPLATE with every `__NAME__` replaced by its value.
# Placeholders the template does not use are ignored; placeholders left over are not an error here (the caller's own
# tests assert none remain). The value is escaped for sed, so a URL with `&` or `|` in its query string lands
# verbatim; a newline is refused (BadTemplateValue).
rz_render_template() {
  local template="$1" out="$2"; shift 2
  [[ -f "$template" ]] || { echo "TemplateMissing: $template does not exist" >&2; return 1; }
  local args=() pair name value
  for pair in "$@"; do
    name="${pair%%=*}"; value="${pair#*=}"
    value="$(rz_sed_escape "$value")" || { echo "BadTemplateValue: the value for $name contains a newline" >&2; return 1; }
    args+=(-e "s|__${name}__|${value}|g")
  done
  sed "${args[@]}" "$template" > "$out"
}

# rz_render_sequencer_config EXAMPLE OUT key=value... -> OUT is EXAMPLE with each `key = ...` line replaced by
# `key = value`. The value is written as given, so a string is passed with its quotes: 'rpc_addr="0.0.0.0:9944"'.
# A key the example does not have is refused (UnknownProfileKey): a typo in chain.toml must not become a no-op.
rz_render_sequencer_config() {
  local example="$1" out="$2"; shift 2
  [[ -f "$example" ]] || { echo "ExampleConfigMissing: $example does not exist (run from a checkout that has crates/rome-zk-sequencer)" >&2; return 1; }
  local args=() pair key value
  for pair in "$@"; do
    key="${pair%%=*}"; value="${pair#*=}"
    if ! grep -qE "^${key} = " "$example"; then
      echo "UnknownProfileKey: '$key' is not a setting in $example" >&2; return 1
    fi
    value="$(rz_sed_escape "$value")" || { echo "BadTemplateValue: the value for $key contains a newline" >&2; return 1; }
    args+=(-e "s|^${key} = .*|${key} = ${value}|")
  done
  sed "${args[@]}" "$example" > "$out"
}

# rz_profile_gas_limit TOML -> sub_block_gas_limit x sub_blocks_per_block, read from a sequencer config. The genesis
# gas limit MUST equal this number: every sealed block carries it, and a stock reth node refuses a jump of more than
# 1/1024 from its parent, so block 1 is only valid if genesis already has it.
rz_profile_gas_limit() {
  python3 - "$1" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
def val(key):
    m = re.search(r'^\s*' + key + r'\s*=\s*([0-9_]+)', text, re.M)
    if not m:
        sys.exit(f"{key} not found in {sys.argv[1]}")
    return int(m.group(1).replace('_', ''))
print(val('sub_block_gas_limit') * val('sub_blocks_per_block'))
PY
}

# rz_hex_body FILE -> the file's contents as bare hex (no 0x, no whitespace); refuses empty, odd-length or non-hex.
rz_hex_body() {
  local file="$1" body
  [[ -s "$file" ]] || { echo "HexFileMissing: $file is absent or empty" >&2; return 1; }
  body="$(tr -d '[:space:]' < "$file")"
  body="${body#0x}"; body="${body#0X}"
  if [[ -z "$body" || ! "$body" =~ ^[0-9a-fA-F]+$ ]] || (( ${#body} % 2 != 0 )); then
    echo "HexFileInvalid: $file does not contain valid hex (odd length or non-hex characters)" >&2; return 1
  fi
  printf '%s' "$body"
}

# rz_genesis_diff_summary OLD NEW -> a one-line list of the top-level fields and alloc entries that differ.
rz_genesis_diff_summary() {
  python3 - "$1" "$2" <<'PY'
import json, sys
old = json.load(open(sys.argv[1]))
new = json.load(open(sys.argv[2]))
diffs = []
for key in sorted((set(old) | set(new)) - {"alloc"}):
    if old.get(key) != new.get(key):
        diffs.append(key)
old_alloc = old.get("alloc", {})
new_alloc = new.get("alloc", {})
for addr in sorted(set(old_alloc) | set(new_alloc)):
    if old_alloc.get(addr) != new_alloc.get(addr):
        diffs.append(f"alloc[{addr}]")
print(", ".join(diffs) if diffs else "(none — byte-identical apart from key ordering?)")
PY
}

# rz_config_value FILE KEY -> the value of the first top-level-or-table `KEY = value` line in a rendered TOML file,
# with surrounding quotes and underscores-in-numbers left as written. Nothing if the key is absent.
rz_config_value() {
  grep -E "^$2 = " "$1" 2>/dev/null | head -1 | sed -E "s/^$2 = //" || true
}

# rz_ensure_jwt FILE [MODE] -> create the engine-API shared secret (the verifier node and derive must agree on one)
# unless the file exists. An existing secret is never replaced: a new one strands the pair. MODE defaults to 600; a
# caller whose containers run as a different user than the one who owns the file passes 644 (the engine port is never
# published, so the secret only matters on the private compose network).
rz_ensure_jwt() {
  local file="$1" mode="${2:-600}"
  if [[ -f "$file" ]]; then
    echo "$file already exists — leaving it (the verifier node and derive must agree on one secret; a new one strands the pair)"
  else
    (umask 077; openssl rand -hex 32 > "$file")
    chmod "$mode" "$file"
    echo "generated $file (engine API secret shared by the verifier node and derive; never commit it)"
  fi
}
