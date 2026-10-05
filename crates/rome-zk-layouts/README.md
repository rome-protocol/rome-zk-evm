# rome-zk-layouts

Byte-exact account layouts for every account the inbox and settlement programs read or write — the inbox
chunk account, the inbox batch account, the per-chain batch cursor, the settlement root account, the
per-batch pending account, the verifier registry, the chain and global configuration accounts, the
permissionless-chain-id nonce, and the reserved-id allow marker — the channel/frame header the batcher
prepends to every DA frame — plus the accumulator's commitment formula, the forced-lane's empty-root
domain constant, and (for exits) the `exit` module below. Every pubkey field in a decoded struct is a raw
`[u8; 32]`, not a typed `Pubkey` — a caller that wants the typed value wraps the bytes itself. The one
exception is PDA derivation (below), which needs the real `solana_program::pubkey::Pubkey` type and so is
this crate's only reason to depend on `solana-program` (pinned, SBF-compatible); every other function
stays free of it.

## Cargo features

| feature | default | gates | what a consumer without it gets |
|---|---|---|---|
| `solana` | **on** | `solana-program`, every `pda()`/`seeds()` helper that needs a real `Pubkey` (11 modules, 31 sites — `perm_nonce::seeds` too, since it takes an authority `Pubkey`) | every byte-level constant (`OFF_*`/`LEN`/`MAGIC`/`VERSION`), `read`/`write`, `acc`, `forced_empty_root`, `FORCED_EMPTY_DOMAIN`, `frame`, `chunk`'s header layout, `public_values` (incl. `pack_zisk_outputs`/`unpack_zisk_outputs`), `chainid`, `LayoutError` — the whole byte-level surface, pure and unconditional |

Every existing consumer (both programs, both clients, the batcher, derive, `rome-zk-pda`, `rome-zk-testkit`)
keeps the default feature and is unaffected — Cargo default features are on unless a dependant opts out.
Only [`rome-zk-channel`](../rome-zk-channel) (`default-features = false`, it never touches a `Pubkey`) and
the ZisK bench guest ([`guest/rome-zk-bench-decode`](../../guest/rome-zk-bench-decode)) turn it off, which is
what lets both build for the `riscv64ima-zisk-zkvm-elf` target: `solana-program` pulls
`solana-secp256k1-recover` → `libsecp256k1` → `rand 0.7` → `getrandom 0.1.16`, which has no backend for that
target. `cargo check -p rome-zk-layouts --no-default-features` is a required CI check (`.github/workflows/ci.yml`),
not proved only by the guest's own `cargo-zisk build`.

## What it guarantees

- **One definition, both sides.** [`programs/zk-inbox`](../../programs/zk-inbox) and
  [`programs/zk-settlement`](../../programs/zk-settlement) read and write these exact byte offsets on
  chain; [`zk-inbox-client`](../zk-inbox-client) and [`zk-settlement-client`](../zk-settlement-client)
  decode the identical bytes off chain. A field reorder or width change here is felt on every consumer at
  once — there is no second copy of any layout to drift out of sync.
- **One definition of every account's PDA, too.** Each layout module exports `seeds(...)` (the raw seed
  components an `invoke_signed` array needs, plus its bump) and `pda(program_id, ...) -> (Pubkey, u8)`.
  Both programs and both clients resolve to these functions — directly, or through a thin same-name
  wrapper kept so an existing call site (an instruction builder, a test, an example) needs no further
  change. A program deriving a PDA to create it and a client deriving the same PDA to build a transaction
  against it can never silently disagree, because there is only one derivation for either of them to call.
- **All integers are little-endian**, consistently, across every layout in this crate.
- The accumulator's `acc` commitment (`keccak(chain_id ‖ batch ‖ open_slot ‖ expected_count ‖ root ‖
  forced_root)`, all integers little-endian) and the forced-lane's empty-root domain constant
  (`keccak("rome-zk/forced/empty/v1")`) are defined exactly once here and golden-tested against an
  independently computed hash, so a future field reorder or endianness slip is caught in this crate's own
  test suite before it ever reaches a consensus-relevant mismatch between the two programs.
- **The batch account header is versioned (`VERSION`, currently 2).** A new field is appended after the
  existing ones, never inserted mid-header, so every existing `OFF_*` stays put and only `HEADER_LEN`
  moves; `read` refuses any other version by name (`BadVersion`) rather than reading a mismatched shape.
  `batch::OFF_OPEN_UNIX_TS` (v2) is the committed `Clock::unix_timestamp` `OpenBatch` writes alongside
  `open_slot` — not part of `acc`, since the accumulator binds only the DA bytes. There is no migration
  path: a version bump is a breaking, non-upgradable change to every account already on chain under the
  old version.
- **The inbox chunk header (`chunk`) and the channel/frame header (`frame`)** are also defined exactly
  once here: `chunk::{MAGIC, HEADER_LEN, OFF_*}` plus `read`/`write_header` for the 64-byte chunk account
  header the inbox program writes and every off-chain reader (the batcher, `zk-inbox-client`) decodes;
  `frame::{FRAME_HEADER_LEN, OFF_*}` plus `read`/`write_header` for the 19-byte frame header
  [`rome-zk-batcher`](../rome-zk-batcher)'s channel codec prepends to every frame body (the zstd/RLP
  codec itself stays in the batcher — only the header shape is shared). Each header's byte-golden test
  uses a literal byte vector, never the offset constants under test, so a future offset change is caught
  even when the crate's own `read`/`write_header` still agree with each other.

## Exits: the `exit` module

`src/exit.rs` is where the byte-level side of exits lives
— no proof verification (that is [`rome-zk-mpt`](../rome-zk-mpt), a sibling crate) and no instruction
handler (that is `programs/zk-settlement`'s `ProveExit`/`ConsumeExit`/`ProposeExitConfig`/
`ActivateExitConfig`, which live in the program):

- **`ExitMessage`** — the fields `RomeExitPortal.initiateExit` (`contracts/exit-portal`) commits.
  `message_preimage()` reproduces the contract's exact 160-byte `abi.encode(uint256, address, bytes32,
  address, uint256)`; `message_hash()`/`storage_slot()` reproduce `keccak256(preimage)` and the
  `sentMessages` mapping's storage slot. All three are pinned byte-for-byte against
  [`fixtures/exit/message_hash.json`](../../fixtures/exit/message_hash.json) (the Solidity fixture) — a
  drift on either side (a Solidity ABI change here, or ours) is caught by that one test.
- **Four account layouts**: `exit_config` (142 B, the portal address/bridge program/pending cap-and-bond
  values, an activation-delayed governance target), `exit_record` (169 B, one per proved-but-unreleased
  exit), `exit_window` (36 B, one per challenge-window-length cap bucket), `exit_nullifier` (1,044 B, a
  1,024-byte persistent replay bitmap covering 8,192 exits per page). Same `read`/`write`/`seeds`/`pda`
  shape as every other layout in this crate.
- **Nullifier paging**: `nullifier_page(nonce) = nonce >> 13`, `nullifier_bit(nonce) = nonce & 8191`;
  `bit_is_set`/`set_bit` take the page they believe they are touching and refuse (`NullifierPageMismatch`)
  before reading or writing a single bit if that page does not actually own the nonce's bit.
- **`cap_units`**: converts a wei amount to whole gwei units (`CAP_UNIT_WEI = 1e9`), rounding **up** — the
  unit `exit_cap_per_window` (root account, see below) counts in.
- **`exit_consumer_seeds`/`exit_consumer_pda`** `["exit_consumer", chain_id]`, derived UNDER
  the caller-supplied `bridge_program` — never under the settlement program itself. `ConsumeExit`'s whole
  authorisation check is `bridge_signer.key == exit_consumer_pda(chain_id, exit_config.bridge_program)`
  with `bridge_signer.is_signer` (only satisfiable via that program's own `invoke_signed` CPI, never a
  keypair); the later `zk-bridge` program's `ReleaseExit` derives the identical PDA to sign that CPI. One
  owner for the derivation keeps both sides agreeing byte-for-byte.

`root.rs`'s doc comment records what its two existing fields count in for exits: `exit_cap_per_window` is
gwei of the native asset per challenge window (`0` = disabled), `poster_bond` is lamports (a number only for
now — no escrow or unit reconciliation yet). Neither field's byte offset changed.

## Deployed-address pins

`tests/pda_pins.rs` derives the PDAs Tiber's already-deployed programs use — five already-live accounts
(root, batch cursor, global config, reserved-allow marker, chain config, chain id 200101) plus four
exit accounts (`exit_config`, `exit_window` window 0, `exit_nullifier` page 0, `exit_record` for a fixed
message hash) **derived under the same live settlement program id but not yet sent anywhere** — and
asserts each one equals a pinned literal address. Everything else in this crate — the byte goldens, and
the delegation tests in the programs and clients that compare their own derivation against this crate's —
agrees with a wrong seed just as readily as it agrees with a right one, because they all resolve to the
same function; this file is the one place a seed change that would strand a live account (or silently
retarget an address nothing has been sent to yet) is caught. The five live pins move only alongside a
Tiber chain reset that redeploys the programs under new addresses; the four exit pins move once
`ProposeExitConfig`/`ProveExit` are actually sent against Tiber.

## Config

None — this is a pure, `no_std`-friendly library crate with no runtime configuration.

## How to test

```sh
cargo test -p rome-zk-layouts
# ZisK-target-safe build (no solana-program in the graph) — required in CI:
cargo check -p rome-zk-layouts --no-default-features
```

## Examples

```sh
cargo run -p rome-zk-layouts --example print_pdas -- <inbox_program_id> <settlement_program_id> <chain_id>
```

Prints the five chain-scoped PDAs (root, batch cursor, global config, reserved-allow marker, chain
config) in both Markdown-table shape (the "PDAs" table in the operator's deploy config) and Rust-literal shape
(`tests/pda_pins.rs`'s five pinned addresses), so an operator moving a chain from one set of deployed
program ids to another (a chain reset) updates both places from the same run instead of hand-deriving
either. Used by the operator's chain-registration script right after registering a chain, and by
the "batches advancing" health check to derive the batch-cursor PDA.

## Security notes

Every layout here is read by an on-chain program under a compute budget and by an off-chain client with no
such constraint; both must agree byte-for-byte on where every field lives, or the two sides silently
diverge on what an account means. The golden tests in `src/lib.rs` (the `acc` formula, the forced-empty
root) and in each layout's own module pin concrete hex values computed independently of this crate's own
code, specifically so a change that happens to keep every existing test passing by accident (a
compensating pair of offset changes, for instance) still fails the fixed-value check.

- **A registry entry names its ZisK release in its `scheme` byte.** `0` is Groth16; `1` is ZisK 1.2.0-alpha
  (`SCHEME_ZISK_1_2_0`, the value every entry written so far carries); `2` is ZisK 1.3.1-alpha
  (`SCHEME_ZISK_1_3_1`). `registry::ZISK_RELEASES` lists the releases, a number is never reused, and a later
  release takes the next unused number; a test holds the published rows fixed. No byte of the entry moves.
  The `scheme` byte is the ZisK release number; code that reads or writes an entry names the release it means.
  `registry::find_zisk(d, vkey_hash, at_slot)` finds the one active BN254 entry under any ZisK scheme for a
  verification-key hash (skipping retired and not-yet-active entries) and refuses two active matches
  (`FindZiskError::TwoActiveMatches`), so a hash can never choose between two releases. Settlement's
  `PostRootProved` and the prover's anchor both look keys up with it. The release list here and the one in Veritas
  are two tables; a test in `programs/zk-settlement` fails if their scheme numbers or names differ.

## Scope

This crate covers the inbox batch account (its sizing and growth), the settlement root, pending and registry
accounts, and the Merkle leaf construction the accumulator commits to. The batch account's
growth-by-instruction sizing, the per-chain batch cursor, and the registration-and-revenue accounts
(chain/global config, nonce, allow marker) are explained in each module's own documentation. See
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for how these accounts fit into the overall data flow.

## Depends on

[`rome-zk-merkle`](../rome-zk-merkle) for the `HashV` trait and the Merkle reduction the accumulator's
commitment formula is built on top of; `solana-program` (pinned, SBF-compatible) for the `Pubkey` type
every `pda` function returns.
