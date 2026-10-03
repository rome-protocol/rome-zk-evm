# zk-settlement-client

A thin, async-runtime-agnostic client for [`programs/zk-settlement`](../../programs/zk-settlement):
instruction builders, PDA derivation, and account decoders for the root, pending, registry, chain
configuration, global configuration, permissionless-nonce and reserved-allow accounts. The batch poster and
the inbox program's own chunk-close path both use this crate rather than the settlement program's
instruction types directly, so neither ever needs to depend on that program crate's `no-entrypoint`
feature itself.

## What it guarantees

- **PDA derivation matches the program exactly** — `root_pda`, `registry_pda`, `pending_pda`,
  `chain_config_pda`, `global_config_pda`, `perm_nonce_pda`, `reserved_allow_pda` and `exit_config_pda` all
  compute the identical seeds the on-chain program uses.
- **`derive_permissionless_chain_id` matches the program's own computation, off-chain.** A permissionless
  chain id is `2^32 + keccak(authority ‖ nonce) mod (2^53 − 2^32)`; this crate computes it with a software
  keccak so a caller can know which id it is about to claim, and which accounts to derive from it, before
  ever sending a transaction — the on-chain program independently recomputes the same value from the same
  inputs and rejects any mismatch.
- **Every instruction builder's account list and authority requirement matches the program's own
  instruction definitions** — see each builder's doc comment, and
  [`programs/zk-settlement`](../../programs/zk-settlement)'s own README for the invariants those
  requirements exist to enforce.
- **No async runtime opinions in the library itself** — an RPC client and Tokio are pulled in only behind
  the `devnet-driver` feature, used by this crate's own example binaries
  (`examples/devnet_driver.rs`, `examples/governance.rs`) and by the thin example wrappers over
  [`rome-zk-ops`](../rome-zk-ops) (`examples/register_chain.rs`, `examples/migrate_chain.rs`, and
  `governance`'s exit-config subcommands). The operator commands themselves live in `rome-zk-ops`.
- **`register_chain --permissionless` registers a chain with an empty registry.** The program refuses a
  permissionless chain that brings its own verifier keys, so `rome-zk-ops register` refuses `--layout1-vkey-json` and
  `--zisk-vkey-json` on that path (error `VkeysNotAllowedOnPermissionless`)
  before it reads a key or calls an RPC, and prints that the chain cannot finalize a proved root until Rome
  registers its layout-1 verifier key. The flow is: run `register_chain`, ask Rome to register the key, and
  Rome adds it with `governance set-registry-entry`. `--reserved` still takes both vkey files.
- **`decode_registry_account` reports every entry's activation slot and retired status, not just the
  header.** Each `RegistryEntryView` carries `activation_slot` — `0` on a v1-length account (active since
  genesis, what `InitChainV2` still writes), the real value once `set_registry_entry_ix` has grown the
  account to v2 — and `retired`, true exactly when that slot is the reserved tombstone value `u64::MAX`.
- **`propose_exit_config_ix`/`activate_exit_config_ix`/`decode_exit_config_account`** build and decode the
  exit-config governance pair — see
  [`programs/zk-settlement`](../../programs/zk-settlement)'s README for the delay rule and state machine
  those two instructions enforce. Used by `rome-zk-ops exit-config propose|activate|show`, which
  `examples/governance.rs`'s `propose-exit-config` / `activate-exit-config` / `show-exit-config` forward to.
  Without `--confirm` the CLI builds and signs against a placeholder blockhash and prints the decoded
  instruction, its discriminant and the base64 transaction bytes instead of sending (`--offline` makes no RPC
  call at all). `show` reads the
  *current* portal/bridge from `exit_config` and the *current* cap/bond from `root` (the account that
  actually stores those two numbers), alongside every pending field and an activatable-now/not-yet
  reading against the live slot.
- **`prove_exit_ix`/`decode_exit_record_account`/`decode_exit_window_account`** build
  `ProveExit` and decode the `exit_record`/`exit_window` accounts it writes.
  **`consume_exit_ix`/`exit_consumer_pda`** build `ConsumeExit` — the ix the `zk-bridge`
  program CPIs, signing `bridge_signer` as the `exit_consumer` PDA; this crate never signs it itself, since
  a PDA has no private key. See [`programs/zk-settlement`](../../programs/zk-settlement)'s README for the
  full authorisation/refund contract those two instructions enforce.

## Config

None — this is a pure library crate. Callers supply their own program id, RPC endpoint and signing key; a
signing key is never read or handled by this crate itself.

## How to test

```sh
cargo test -p zk-settlement-client
```

This crate's own unit tests (account round-trip decoding, PDA derivation) need no on-chain program built.
Its instructions are exercised end to end, against the real programs, in
[`programs/zk-settlement`](../../programs/zk-settlement)'s own test suite — see that program's README for
the build step its tests need first.

## Security notes

`derive_permissionless_chain_id`'s off-chain and on-chain computations must agree byte-for-byte — a
mismatch here would mean this crate derives account addresses (root, registry, chain config) for a
different chain id than the one a permissionless `InitChain` call would actually create on-chain, silently
pointing a caller at accounts that were never created. A golden test in
[`rome-zk-layouts`](../rome-zk-layouts) pins the shared formula both sides compute from.

## Design references

The design behind this crate is in the project specification, which is not in this repository. It covers the
root, pending and registry accounts and the `PostRoot`/`PostRootProved`/`FinalizeBatch`/`RootView`
instructions this crate builds against. It also covers the layout-1 public-values struct `PostRootProved`
binds against when a registry entry names it; `InitChainFields` and `migrate_chain_ix`/`set_drift_bound_ix`
carry the chain's `max_drift_secs` argument that layout binds to. The registration-and-revenue
instructions (`InitGlobalConfig`, `AllowReservedId`/`RevokeReservedId`, `SetFee`, `SetTreasury`,
`MigrateChainV2` (`MigrateChain` is retired), `RefundDeposit`, `ReclaimChain`, `SetGlobalConfig`, the
two-step registry-authority rotation, and `SetDriftBound`) mirror the project's registration-and-revenue
decision, also kept outside this repository — see [`programs/zk-settlement`](../../programs/zk-settlement)'s
README for what each of those instructions enforces. Verifier-key rotation with an explicit activation
delay, and explicit retirement, is `set_registry_entry_ix`, built the same way `set_drift_bound_ix` is —
see that README's "Rotating a verifier key" section.

## Depends on

[`programs/zk-settlement`](../../programs/zk-settlement) itself (built with its `no-entrypoint` feature);
[`rome-zk-layouts`](../rome-zk-layouts) for the shared account layouts. Optionally depends on
[`zk-inbox-client`](../zk-inbox-client) (behind the `devnet-driver` feature) for its example binaries,
which need to derive inbox-side accounts too. Consumed by the batch poster and by the inbox program's own
chunk-close path.
