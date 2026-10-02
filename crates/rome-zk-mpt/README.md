# rome-zk-mpt

A bounded Ethereum Merkle-Patricia (MPT) proof verifier: `verify_account` proves an account's fields
against a proof-bound `state_root`; `verify_storage` proves a storage slot's value — or its provable
absence — against an account's `storage_root`. Built for exits: an exit proof is exactly an account proof of
the L2 exit portal plus a storage proof of `sentMessages[message_hash]`, but this crate names neither "exit"
nor "portal" anywhere — it is a general, two-call Ethereum MPT verifier with no knowledge of what calls it.

## What a proof is

Given a trusted 32-byte root and a 32-byte key (`keccak256(address)` for an account, `keccak256(slot)` for
a storage value), the proof is the ordered list of trie nodes from the root down to the value: `nodes[0]`'s
own `keccak256` must equal the root, each subsequent `nodes[i]`'s `keccak256` must equal the hash the
previous node referenced it by, and the accumulated path (nibbles consumed by each branch/extension node
along the way) must match the key. A node whose own encoding is under 32 bytes is embedded directly in its
parent instead of referenced by hash (the Yellow Paper's "inline child" rule) — this verifier follows that
embedded node's bytes with no separate hash check, since its integrity is already covered by its parent's.

A leaf or extension node's own path is stored hex-prefix ("compact") encoded: the first byte's high
nibble carries the leaf/extension flag and an odd/even nibble-count flag; an odd count folds its first
nibble into that same byte's low nibble, an even count leaves that low nibble as padding. This crate
refuses a non-canonical encoding whose even-path padding nibble is nonzero — a real Ethereum client never
emits one, so this is belt-and-suspenders canonical-form enforcement, not a soundness requirement (every
node is keccak-bound to a trusted root regardless).

## Bounds, and why

Every proof is checked against two bounds **before a single byte is hashed or RLP-decoded**:

- `MAX_NODES = 64` — no real Ethereum trie needs anywhere near this many nodes on a path; a proof claiming
  more is refused outright (`TooManyNodes`), never truncated.
- `MAX_NODE_BYTES = 532` — a branch node's worst case: 16 child slots × (1 RLP header byte + a 32-byte
  hash) = 528 B, plus a short value slot and RLP list-header overhead. A node over this (`NodeTooLarge`)
  is refused before its bytes are interpreted at all.

These bounds exist because `ExitProof` (below) is the wire shape a Solana instruction carries — a caller
budgeting against the V1 transaction envelope needs a hard ceiling on how large a proof can ever be, not
just what a particular fixture happens to measure.

## Defensive value-shape guards

Once a proof's walk reaches a value, the leaf value bytes themselves are re-validated: a storage value
over 32 bytes (`ValueTooLong`) and an account whose RLP shape isn't the fixed 4-item
`[nonce, balance, storage_root, code_hash]` list (wrong item count, an oversized nonce/balance, a
wrong-length `storage_root`/`code_hash`, or a nested list where a scalar is expected — all `BadAccountRlp`
or `ValueTooLong`) are refused by name rather than reaching an unchecked `copy_from_slice`. None of this
is reachable through a real `eth_getProof` fixture (go-ethereum never writes a malformed account or an
over-length value), so `storage_value_over_32_bytes_is_refused` and `account_rlp_adversarial_refused`
hand-build hash-consistent single-leaf tries to exercise it directly. A related non-canonical case that
*is* correct behaviour, not a refusal — a leading-zero storage value (`0x00 01`) still decodes to the
correct left-padded `Present(1)` (`leading_zero_storage_value_decodes_to_one`), which is what lets a
caller compare the result to `1` directly.

## Inclusion vs exclusion

`verify_account` has **no exclusion outcome** — its only real caller (`ProveExit`) always asks
about a portal address it already expects to exist (read from `exit_config`, never a caller-supplied
argument), so a path that diverges from the claimed address is refused (`PathMismatch`), never reported as
"account absent". `verify_storage` is the opposite: `StorageValue::Absent` is a distinct, explicit
outcome, never conflated with `Present([0u8; 32])` — a slot that was never written and a slot whose value
happens to be zero are different facts about the trie, and the type system keeps a caller from confusing
them. `fixtures/exit/anvil_getProof_unsent.json` is exactly this case: a message that was never sent, same
shape as a real proof, `storageProof[0].value == "0x0"`.

## What this crate does NOT check

Everything here assumes `state_root`/`storage_root` are already roots the caller trusts. The root's own
provenance — that it really is a finalized batch's proof-bound `state_root`, read from the right
pending/root PDA for the right chain and batch — is entirely `programs/zk-settlement`'s job. This crate
has no opinion on where a root came from, and no code path here ever reads from Solana or from the network.

## RLP: this crate's own bounded decoder, not `alloy-rlp`

See `src/rlp.rs`'s module doc for the full reasoning. In short: the security contract here is bound-first
(every node is already size-checked before a byte of it is parsed) and the only shapes this verifier ever
needs to recognise are the four fixed Ethereum trie-node forms — a general-purpose RLP library would decode
arbitrary nesting and length and need the same bounds re-imposed after the fact. The bounded decoder has no
dependency at all, so it carries zero risk on either of this crate's real targets (SBF, and a future zkVM
guest that must stay `solana-program`-free).

## Hashing

Every hash goes through [`rome_zk_merkle::keccak256`](../rome-zk-merkle) — the one keccak owner in the workspace
— never a locally-linked `sha3`/keccak dependency. That function already dispatches to the
`solana_program::keccak::hashv` syscall on `target_os = "solana"` and a pure Rust implementation everywhere
else, so this crate needs no target-specific code of its own for that; it also carries no `solana-program`
dependency of its own (it needs no `Pubkey` — every input/output here is a raw byte array), so it has no
`solana` Cargo feature to gate at all, unlike `rome-zk-layouts`.

## `ExitProof`

```rust
pub struct ExitProof {
    pub account_nodes: Vec<Vec<u8>>,
    pub storage_nodes: Vec<Vec<u8>>,
}
```

The wire shape `ProveExit` carries — an account proof of the configured exit portal plus a
storage proof of `sentMessages[message_hash]` in that account's storage trie. Borsh-encoded (matching
every other on-chain instruction argument in this workspace); `byte_len()` returns the proof's exact
serialized size (computed by actually serializing, not a hand-derived formula) so a caller can budget it
against the V1 envelope (4,096 B) before ever building the instruction.

## Fixtures this crate's tests read

All from `fixtures/exit/` (never regenerated silently — see that directory's own
README for provenance):

| fixture | what it proves | node counts (measured) |
|---|---|---|
| `anvil_getProof.json` + `anvil_state_root.json` + `message_hash.json` | one `initiateExit` on a fresh anvil portal — inclusion | 3 account nodes, 2 storage nodes; max 308 B |
| `anvil_getProof_unsent.json` | the same portal, a message that was never sent — exclusion | 3 account + 1 storage node |
| `tiber_eoa_getProof.json` + `tiber_eoa_block.json` | a real Tiber verifier account proof of an EOA (`0xCec1…E11c`) — one-node "leaf as root" trie | 1 account node, 120 B |
| `anvil_getProof_multi.json` + `anvil_state_root_multi.json` + `message_hash_multi.json` | the same portal contract after 20 `initiateExit` calls — a real multi-key trie | 3 account nodes; 5 storage nodes across the two keyed proofs (exit 7 inclusion + a never-sent nonce exclusion); max 308 B |
| `anvil_getProof_ext.json` + `anvil_state_root_ext.json` + `message_hash_ext.json` | the same portal contract after 500 `initiateExit` calls — deep enough to reliably contain real Extension nodes | 3 account nodes; 8 storage nodes across the two keyed proofs (nonce 21 inclusion — Branch, Branch, Extension, Branch, Leaf — + nonce 516 exclusion — Branch, Branch, Extension); max 532 B |

The 20-exit trie's actual node-kind composition (branch/leaf/extension) is **measured, not assumed** —
`multi_fixture_node_kinds_are_measured_not_assumed` classifies every node in that fixture and asserts what
is actually there. It found branch and leaf nodes only: with just 20 sparse `keccak256`-derived keys, no
two keys in this particular set share a long-enough common prefix to need an extension node, and no node
here is small enough to embed inline. `inline_child_is_followed` therefore exercises the inline-child code
path against a small, deterministic, hand-built two-node trie instead — independent of what any real
anvil trie happens to produce, and unaffected by a future fixture regeneration.

The 500-exit `ext` fixture closes the extension-node gap the 20-exit trie leaves open:
`ext_fixture_has_an_extension_node` asserts (again measured, not assumed) that it contains at least one
real Extension node; `inclusion_through_extension_verifies` proves a real inclusion proof that walks
*through* one to a `Present(1)` leaf, and `exclusion_diverging_inside_extension_is_absent` proves a real
exclusion proof that *diverges at* one, both landing on committed fixture bytes rather than a scratch
test that could be lost. `hand_built_extension_divergence_and_missing_child` additionally covers three
shapes a single-nibble real-world extension cannot exercise on its own, against a small hand-built
3-nibble extension node: divergence strictly inside the path, a key shorter than the path (proving the
length guard runs before any out-of-bounds slice), and a full match into a child the proof does not
carry. No committed fixture (real or hand-built) produces an inline child inside a *multi-key* trie —
`inline_child_is_followed`'s hand-built two-node trie remains the only coverage of that path, and stays
correctly kept even though nothing here measures it as "reachable in this fixture set".

## How to test

Heavy work (`cargo test`, `cargo check`, `cargo build-sbf`) runs on a build machine, never a laptop:

```sh
cargo test -p rome-zk-mpt
cargo clippy --workspace --all-targets -- -D warnings
```

SBF build (proves the target compiles with the syscall keccak path active — CU is measured in
`ProveExit`, on real BPF, not here):

```sh
cargo build-sbf --manifest-path crates/rome-zk-mpt/Cargo.toml
```

## Design references

The exit proof is MPT over the proof-bound `state_root`, with node bounds; CU is unmeasured here by design, see
above. See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for how this crate fits between the exit portal
contract and `ProveExit`.

## Depends on

[`rome-zk-merkle`](../rome-zk-merkle) for `keccak256`; `borsh` for `ExitProof`'s wire encoding. No
`solana-program` dependency.
