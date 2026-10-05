#!/usr/bin/env bash
# scripts/tests/vkey_register_wrapper.sh — scripts/vkey-register.sh refuses what it can refuse by name before it runs
# anything, and passes the rest to `rome-zk-ops vkey register` (global flags first, the release, the guest-build directory
# of this checkout, the proving keys from --proving-key-dir or ZISK_HOME). The binary is a stub that records its arguments;
# so the flags it records are also checked against what the real command takes: always against the argument struct in
# crates/rome-zk-ops/src/cli.rs, and, when a built rome-zk-ops is at hand (ROME_ZK_OPS_REAL, or target/debug|release), by
# running the real parser on them.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WRAP="$SCRIPT_DIR/../vkey-register.sh"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

printf '#!/usr/bin/env bash\necho "$*" >> "%s/calls"\nexit 0\n' "$WORK" > "$WORK/ops"; chmod +x "$WORK/ops"
export ROME_ZK_OPS="$WORK/ops"
echo '{}' > "$WORK/genesis.json"; mkdir "$WORK/keys"
S=11111111111111111111111111111111
HEX=$(printf 'ab%.0s' $(seq 32))
base0=(--settlement $S --chain-id 77 --genesis "$WORK/genesis.json" --guest-tag v0.2.0 --elf-sha256 $HEX --program-vk $HEX
       --registry-keypair /k/r.json --payer-keypair /k/p.json)
base=("${base0[@]}" --zisk 1.3.1-alpha)

refuses() { # name, args...
  local name="$1" out; shift; : > "$WORK/calls"
  if out="$(env -u ZISK_HOME "$WRAP" "$@" 2>&1)"; then fail "refuses $name" "exited 0"
  elif ! grep -q "^$name" <<<"$out"; then fail "refuses $name" "$out"
  elif [[ -s "$WORK/calls" ]]; then fail "refuses $name" "the binary ran: $(cat "$WORK/calls")"
  else pass "refuses $name and runs nothing"; fi
}

refuses UnknownArgument "${base[@]}" --colour blue
refuses FlagValueMissing --settlement
refuses ChainIdMissing --settlement $S
refuses ZiskMissing "${base0[@]}" --activation-slot 5 --proving-key-dir "$WORK/keys"
refuses GenesisUnreadable "${base[@]/$WORK\/genesis.json//nonexistent.json}" --activation-slot 5 --proving-key-dir "$WORK/keys"
refuses ActivationMissing "${base[@]}" --proving-key-dir "$WORK/keys"
refuses ActivationConflict "${base[@]}" --activation-slot 5 --activation-delay-slots 5 --proving-key-dir "$WORK/keys"
refuses ProvingKeyDirMissing "${base[@]}" --activation-slot 5
refuses ProvingKeyDirMissing "${base[@]}" --activation-slot 5 --proving-key-dir "$WORK/none"
refuses ProvingKeyDirMissing "${base[@]}" --activation-slot 5 --proving-key-dir "$WORK/keys" --anchor-zisk 1.2.0-alpha --anchor-proving-key-dir "$WORK/none"

: > "$WORK/calls"
if "$WRAP" "${base[@]}" --activation-delay-slots 30 --proving-key-dir "$WORK/keys" --rpc-url http://127.0.0.1:1 --confirm >/dev/null 2>&1 \
   && c="$(cat "$WORK/calls")" \
   && [[ "$c" == --rpc-url\ http://127.0.0.1:1\ --confirm\ vkey\ register\ * && "$c" == *"--proving-key-dir $WORK/keys"* \
      && "$c" == *"--guest-build-dir "*"/deploy/rollup/guest-build"* && "$c" == *"--activation-delay-slots 30"* && "$c" == *"--zisk 1.3.1-alpha"* ]]; then
  pass "a complete request reaches 'vkey register' with the global flags first"
else fail "a complete request reaches 'vkey register'" "$(cat "$WORK/calls")"; fi

: > "$WORK/calls"
if "$WRAP" "${base[@]}" --activation-slot 9 --proving-key-dir "$WORK/keys" --anchor-guest-tag v0.1.0 --anchor-zisk 1.2.0-alpha --anchor-proving-key-dir "$WORK/keys" --anchor-evm-tag v0.1.9 --bridge-program 27TbMDUyVynpFpqeKygpUMcDzWKHfW4k9aRN5yCysLEQ --move-pending-activation >/dev/null 2>&1 \
   && c="$(cat "$WORK/calls")" && [[ "$c" == *"--anchor-guest-tag v0.1.0"* && "$c" == *"--anchor-zisk 1.2.0-alpha"* && "$c" == *"--anchor-proving-key-dir $WORK/keys"* && "$c" == *"--anchor-evm-tag v0.1.9"* && "$c" == *"--bridge-program 27TbMDUy"* && "$c" == *"--move-pending-activation"* ]]; then
  pass "--anchor-guest-tag, --anchor-zisk, --anchor-proving-key-dir, --anchor-evm-tag, --bridge-program and --move-pending-activation reach 'vkey register'"
else fail "the anchor flags, --bridge-program and --move-pending-activation reach 'vkey register'" "$(cat "$WORK/calls")"; fi
cp "$WORK/calls" "$WORK/full_call"   # every flag the wrapper can pass, for the checks against the real command below
: > "$WORK/calls"
if "$WRAP" "${base[@]}" --activation-slot 9 --proving-key-dir "$WORK/keys" >/dev/null 2>&1 \
   && c="$(cat "$WORK/calls")" && [[ "$c" != *"--anchor-guest-tag"* && "$c" != *"--anchor-zisk"* && "$c" != *"--anchor-proving-key-dir"* && "$c" != *"--anchor-evm-tag"* && "$c" != *"--bridge-program"* && "$c" != *"--move-pending-activation"* ]]; then
  pass "neither flag is passed when not given"
else fail "neither flag is passed when not given" "$(cat "$WORK/calls")"; fi
refuses FlagValueMissing "${base[@]}" --activation-slot 9 --proving-key-dir "$WORK/keys" --anchor-guest-tag

: > "$WORK/calls"; mkdir -p "$WORK/zh/provingKey"
if ZISK_HOME="$WORK/zh" "$WRAP" "${base[@]}" --activation-slot 9 >/dev/null 2>&1 && grep -q -- "--proving-key-dir $WORK/zh/provingKey" "$WORK/calls" && ! grep -q -- "--confirm" "$WORK/calls"; then
  pass "ZISK_HOME supplies the proving keys; without --confirm nothing is confirmed"
else fail "ZISK_HOME supplies the proving keys" "$(cat "$WORK/calls")"; fi

: > "$WORK/calls"
if "$WRAP" show --settlement $S --chain-id 77 >/dev/null 2>&1 && [[ "$(cat "$WORK/calls")" == "vkey show --settlement $S --chain-id 77" ]]; then pass "show is a read"
else fail "show is a read" "$(cat "$WORK/calls")"; fi
refuses UnknownArgument show --settlement $S --chain-id 77 --guest-tag x

# ---- the flags are the real command's -------------------------------------------------------------------------------------
# The stub takes any flag, so what it records proves only what the wrapper sends. These two checks hold that against the command.
CLI="$SCRIPT_DIR/../../crates/rome-zk-ops/src/cli.rs"
if python3 - "$CLI" "$WORK/full_call" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r"pub struct VkeyRegisterArgs \{\n(.*?)\n\}", src, re.S)
if not m:
    sys.exit("cli.rs has no VkeyRegisterArgs")
fields, attrs = {}, []
for line in m.group(1).splitlines():
    t = line.strip()
    if t.startswith("#[arg"):
        attrs.append(t)
    elif t.startswith("pub "):
        name, ty = re.match(r"pub (\w+): (.+),$", t).groups()
        fields["--" + name.replace("_", "-")] = (ty, " ".join(attrs))
        attrs = []
    elif not t.startswith("///"):
        attrs = []
call = open(sys.argv[2]).read().split()
i = call.index("register")
sent = {a for a in call[i + 1:] if a.startswith("--")}
unknown = sorted(sent - set(fields))
required = sorted(f for f, (ty, a) in fields.items() if not ty.startswith("Option<") and ty != "bool" and "default_value" not in a)
unsent = sorted(set(required) - sent)
if unknown or unsent:
    sys.exit(f"flags the wrapper sends that 'vkey register' does not take: {unknown}; required flags the wrapper does not send: {unsent}")
PY
then pass "every flag the wrapper sends is one 'vkey register' takes, and every flag it requires is sent (cli.rs)"
else fail "the wrapper's flags against 'vkey register' in cli.rs" "see above"; fi

REAL="${ROME_ZK_OPS_REAL:-}"
for cand in "$SCRIPT_DIR/../../target/debug/rome-zk-ops" "$SCRIPT_DIR/../../target/release/rome-zk-ops"; do
  [[ -n "$REAL" || ! -x "$cand" ]] || REAL="$cand"
done
if [[ -z "$REAL" || ! -x "$REAL" ]]; then
  echo "SKIP: the real rome-zk-ops is not built (set ROME_ZK_OPS_REAL, or build it); the flags were checked against cli.rs only"
else
  # The shim runs the real binary offline on the arguments the wrapper builds: its parser sees every flag, and the first
  # refusal it names shows which flag arrived. Nothing is read from a chain.
  printf '#!/usr/bin/env bash\nexec "%s" --offline "$@"\n' "$REAL" > "$WORK/real"; chmod +x "$WORK/real"
  real() { ROME_ZK_OPS="$WORK/real" "$WRAP" "${base0[@]}" --activation-slot 9 --proving-key-dir "$WORK/keys" "$@" 2>&1; }
  out="$(real --zisk 9.9.9-alpha)"
  grep -q '^ZiskVersionUnknown' <<<"$out" && grep -q '9.9.9-alpha' <<<"$out" && pass "the real command receives --zisk (an unknown release is refused by name)" || fail "the real command receives --zisk" "$out"
  out="$(real --zisk 1.2.0-alpha)"
  grep -q '^ZiskVersionNotOpen' <<<"$out" && pass "the real command refuses a release that takes no new key" || fail "the real command refuses a withdrawn release" "$out"
  out="$(real --zisk 1.3.1-alpha --anchor-proving-key-dir "$WORK/keys")"
  grep -q '^AnchorProvingKeyDirUnused' <<<"$out" && pass "the real command receives --anchor-proving-key-dir" || fail "the real command receives --anchor-proving-key-dir" "$out"
  out="$(real --zisk 1.3.1-alpha --anchor-zisk 1.2.0-alpha --anchor-proving-key-dir "$WORK/keys" --anchor-guest-tag v0.1.0 --anchor-evm-tag v0.1.9)"
  if grep -qE '^error:|^AnchorProvingKeyDirUnused' <<<"$out"; then fail "the real command takes --anchor-zisk with its own keys and the other anchor flags" "$out"
  else pass "the real command takes --anchor-zisk with its own keys and the other anchor flags"; fi
fi

exit $FAILED
