# rome-zk-merkle

This workspace's **one** `keccak256`, plus a hash-agnostic binary
Merkle-tree reduction built on top of it. `keccak256` dispatches on `target_os`: the syscall-backed
`solana_program::keccak::hashv` on-chain (cheap in compute units — no software keccak on Solana's BPF
runtime), a pure-Rust `sha3::Keccak256` everywhere else — both are golden-tested to agree byte-for-byte.
Every program and client that needs a keccak imports this function instead of hand-rolling its own; the
Merkle reduction itself keeps **no opinion on which keccak implementation computes a hash** — every
function takes the hash function as a caller-supplied closure shaped exactly like `keccak256`
(`Fn(&[&[u8]]) -> [u8; 32]`), so a caller passes `keccak256` itself (or, in this crate's own tests, another
function of the same shape, to keep the tests independent of the production implementation they check).

## What it guarantees

- **One keccak256, everywhere.** Before this crate owned it, `software_keccak` (a hand-rolled pure-Rust
  keccak, used for exactly the same purpose) was defined independently in four places — this crate's own
  tests, `rome-zk-layouts`, `zk-inbox-client`, and a `zk-settlement` test file — any one of which could
  have silently drifted from the on-chain syscall path. Now there is exactly one production `keccak256`;
  every former copy imports it.

- **Construction:** `hash_pair(left, right) = keccak256(left ++ right)`; an odd level's last node is
  paired with a domain-separated `EMPTY` sentinel (`keccak256(b"rome-zk/merkle/empty/v1")`), never
  duplicated against itself. An empty leaf list's root is `[0u8; 32]`; a single-leaf list's root is that
  leaf, unreduced.
- **Leaves are position-bound.** [`indexed_leaf`] computes `keccak256(idx_le[4] ‖ hash)`, binding a leaf to
  its position so it cannot be silently reordered without changing its own hash — this is the leaf value
  the inbox program's batch accumulator actually commits, not the bare chunk hash.
- **`root` and `root_in_place` agree.** `root_in_place` reduces leaves stored in a raw byte buffer
  in-place — the shape the on-chain program's account data actually has (`&mut [u8]`, not
  `Vec<[u8; 32]>`) — and is tested against the allocating `root` for every leaf count the on-chain tests
  exercise.
- **Matches the sequencer's independent copy.** [`rome-zk-sequencer`](../rome-zk-sequencer) carries its
  own Merkle implementation for its sub-block transaction root (a different domain — sub-block tx roots,
  not inbox chunks); a golden test in this crate pins concrete hex roots and reproduces them, proving the
  two constructions agree byte-for-byte on the underlying reduction rule.

## Config

None — pure library, no runtime configuration.

## How to test

```sh
cargo test -p rome-zk-merkle
```

## Security notes

The odd-level padding rule matters: padding with a duplicate of the last node (a construction some Merkle
tree implementations use) lets an attacker with control over leaf count sometimes produce two different
leaf sets with the same root. This crate pads with a fixed, domain-separated sentinel instead, and a test
asserts the odd-level case actually takes that path rather than silently duplicating. The golden tests
here are checked against hash values computed independently of this crate's own code, so a change that
would otherwise appear self-consistent (every existing test still passes) is still caught if it changes
what the fixed, pinned values were.

## Design references

Leaf construction is `keccak(idx_le[4] ‖ chunk_hash)`; the batch accumulator has this crate's
reduction at its core. Used by both on-chain programs and by every off-chain reader that needs to
verify a batch's Merkle commitment independently.

## Depends on

`solana-program` (pinned, SBF-compatible) for the on-chain keccak syscall; `sha3` (target-gated to
non-`solana` builds only, so `cargo build-sbf` never resolves it) for the off-chain path.
