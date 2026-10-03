#!/usr/bin/env bash
# scripts/tests/no_legacy_tx.sh — Solana transactions this repo builds are V1 only, sent through
# rome-zk-solana-sender. This refuses a legacy transaction or message constructor in client code under crates/ and
# programs/: Transaction::new_signed_with_payer, Transaction::new_with_payer, Transaction::new_unsigned,
# Transaction::new(, Transaction::new_with_compiled_instructions, Message::new(, Message::new_with_blockhash,
# Message::new_with_compiled_instructions, VersionedMessage::Legacy and legacy::Message.
#
# What is not client code, and so not checked:
#   - any file under a tests/ or benches/ directory, and the shared test fixtures crate crates/rome-zk-testkit
#     (a dev-dependency only, it drives solana-program-test);
#   - the lines of each inline test module: a #[cfg(test)] line followed (next non-blank line) by mod name {, up to its
#     closing brace at the module's indent. Code after a test module, nested or not, is still checked, and a
#     #[cfg(test)] on a fn, use or const skips nothing.
# A comment line (// or //!) is not checked either. Run with a directory argument to check another tree (used by the
# self-test below, which plants a line in a copy).
set -uo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1 — $2"; FAILED=1; }

# POSIX extended regex, so it runs the same under macOS and GNU grep. The name before "Transaction" must not be a word
# character, so VersionedTransaction::try_new (which carries a V1 message) is never matched.
PATTERN='(^|[^A-Za-z0-9_])Transaction::(new_signed_with_payer|new_with_payer|new_unsigned|new_with_compiled_instructions|new)([^A-Za-z0-9_]|$)|(^|[^A-Za-z0-9_])Message::(new|new_with_blockhash|new_with_compiled_instructions)[(]|VersionedMessage::Legacy|legacy::Message'

# Prints the file with the lines of every inline test module blanked, so line numbers stay right. A test module is a
# #[cfg(test)] line whose next non-blank line opens an inline module (mod name {); it ends at the closing brace on its
# own line at the module's indent (rustfmt, enforced by the fmt job, puts it there). A #[cfg(test)] on a fn, use or
# const blanks nothing, and code after a test module (nested, or not the last item of the file) is still scanned.
strip_test_modules() {
  awk '
    { line[NR] = $0 }
    END {
      i = 1
      while (i <= NR) {
        if (line[i] ~ /^[[:space:]]*#[[]cfg[(]test[)][]]/) {
          for (j = i + 1; j <= NR && line[j] ~ /^[[:space:]]*$/; j++) {}
          if (j <= NR && line[j] ~ /^[[:space:]]*(pub([(][a-z]+[)])?[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*[{]/ && line[j] !~ /[}][[:space:]]*$/) {
            ind = line[j]; sub(/[^[:space:]].*$/, "", ind)
            for (k = j + 1; k <= NR && line[k] != ind "}"; k++) {}
            for (m = i; m <= k && m <= NR; m++) print ""
            i = k + 1; continue
          }
        }
        print line[i]; i++
      }
    }' "$1"
}

# Prints "file:line:text" for every legacy constructor in the non-test part of one Rust file.
scan_file() {
  local f="$1"
  strip_test_modules "$f" | grep -nE -- "$PATTERN" | grep -vE '^[0-9]+:[[:space:]]*//' | sed "s|^|$f:|"
}

scan_tree() {
  local root="$1" f
  while IFS= read -r f; do
    scan_file "$f"
  done < <(find "$root/crates" "$root/programs" -name '*.rs' \
             -not -path '*/tests/*' -not -path '*/benches/*' -not -path '*/target/*' \
             -not -path "$root/crates/rome-zk-testkit/*" 2>/dev/null | sort)
}

hits="$(scan_tree "$ROOT")"
if [[ -z "$hits" ]]; then
  pass "no legacy transaction or message constructor in client code under crates/ and programs/"
else
  fail "no legacy transaction or message constructor in client code" "send V1 through rome-zk-solana-sender instead:
$hits"
fi

# Self-test: the check must fail on a planted line, and must ignore a comment and a #[cfg(test)] tail. Done on a copy
# of one client crate's source, so the real tree is never touched.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/crates/demo/src" "$TMP/programs"
cat > "$TMP/crates/demo/src/lib.rs" <<'RS'
// Transaction::new_signed_with_payer in a comment is fine.
pub fn ok() {}
#[cfg(test)]
mod tests {
    fn t() { let _ = Transaction::new_signed_with_payer(&[], None, &[], h); }
}
RS
if [[ -z "$(scan_tree "$TMP")" ]]; then pass "self-test: a comment and a #[cfg(test)] tail are not flagged"; else fail "self-test: a comment and a #[cfg(test)] tail are not flagged" "the scan flagged them"; fi
for line in \
  'let tx = Transaction::new_signed_with_payer(&ixs, Some(&p), &[&k], bh);' \
  'let tx = solana_sdk::transaction::Transaction::new_with_payer(&ixs, Some(&p));' \
  'let tx = Transaction::new_unsigned(m);' \
  'let m = Message::new(&ixs, Some(&p));' \
  'let m = solana_message::legacy::Message::new_with_blockhash(&ixs, Some(&p), &bh);' \
  'let m = VersionedMessage::Legacy(m);'
do
  printf 'pub fn ok() {}\n%s\n' "$line" > "$TMP/crates/demo/src/lib.rs"
  if [[ -n "$(scan_tree "$TMP")" ]]; then pass "self-test: refuses a planted line: ${line:0:60}"; else fail "self-test: refuses a planted line" "not caught: $line"; fi
done
# An early #[cfg(test)] on a helper fn must not hide what follows it.
printf 'pub fn ok() {}\n#[cfg(test)]\nfn helper() {}\n\nlet tx = Transaction::new_signed_with_payer(&ixs, Some(&p), &[&k], bh);\n' > "$TMP/crates/demo/src/lib.rs"
if [[ -n "$(scan_tree "$TMP")" ]]; then pass "self-test: a line after an early #[cfg(test)] fn helper is refused"; else fail "self-test: early #[cfg(test)] fn" "line after it was skipped"; fi
# A planted line inside a real test module (also pub(crate) mod, and a blank line before the mod) is not client code.
printf 'pub fn ok() {}\n#[cfg(test)]\n\npub(crate) mod tests {\n    fn t() { let _ = Transaction::new_with_payer(&[], None); }\n}\n' > "$TMP/crates/demo/src/lib.rs"
if [[ -z "$(scan_tree "$TMP")" ]]; then pass "self-test: a line inside a real #[cfg(test)] module is not flagged"; else fail "self-test: real test module" "flagged"; fi
# A line before the test module is still refused, and a helper fn then a real module skips only the module.
printf '#[cfg(test)]\nfn helper() {}\nlet m = Message::new(&ixs, None);\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = Transaction::new_unsigned(m); }\n}\n' > "$TMP/crates/demo/src/lib.rs"
if [[ "$(scan_tree "$TMP" | wc -l | tr -d ' ')" == "1" ]]; then pass "self-test: helper fn then test module: only the line between is refused"; else fail "self-test: helper fn then module" "expected exactly one hit"; fi
# A nested, indented test module must not hide the top-level code after it.
printf 'pub mod outer {\n    #[cfg(test)]\n    mod tests {\n        fn t() {}\n    }\n}\npub fn after() { let m = Message::new(&ixs, None); }\n' > "$TMP/crates/demo/src/lib.rs"
if [[ -n "$(scan_tree "$TMP")" ]]; then pass "self-test: a line after a nested test module is refused"; else fail "self-test: nested test module" "line after it was skipped"; fi
# A mid-file test module (not the last item) must not hide the fn after it.
printf '#[cfg(test)]\nmod helpers {\n    fn h() {}\n}\nasync fn main() { let tx = Transaction::new_unsigned(m); }\n' > "$TMP/crates/demo/src/lib.rs"
if [[ -n "$(scan_tree "$TMP")" ]]; then pass "self-test: a fn after a mid-file test module is refused"; else fail "self-test: mid-file test module" "fn after it was skipped"; fi
# V1 and versioned constructors must not trip it.
printf 'pub fn ok() { let _ = VersionedTransaction::try_new(VersionedMessage::V1(m), &[&k]); }\n' > "$TMP/crates/demo/src/lib.rs"
if [[ -z "$(scan_tree "$TMP")" ]]; then pass "self-test: a V1 VersionedTransaction::try_new is not flagged"; else fail "self-test: V1 try_new" "flagged"; fi

exit $FAILED
