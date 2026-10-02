# zk-inbox-client

A thin, async-runtime-agnostic client for [`programs/zk-inbox`](../../programs/zk-inbox): instruction
builders, PDA derivation functions, account decoders, and an off-chain reference implementation of the
accumulator's commitment formula (so a caller can verify the on-chain `acc` value independently, without
trusting the program's own computation of it). This crate only builds `Instruction`s and decodes bytes —
sending transactions and managing an RPC connection is the caller's job.

## What it guarantees

- **PDA derivation matches the program exactly.** `chunk_pda`, `batch_pda`, `cursor_pda` and `root_pda`
  compute the identical seeds `programs/zk-inbox` uses internally — a caller building an instruction with
  this crate's derived addresses will always match what the program itself expects, by construction, not
  by convention.
- **Instruction builders encode the exact account order and data shape the program's own
  `process_instruction` expects.** Each builder's doc comment states the account list and any
  authority requirement (e.g. `open_chunk_ix` requires its `payer` to be that batch's registered
  authority; `finalize_batch_ix` requires its `authority` parameter to be that same batch's stored
  authority and to sign) directly from the program's own instruction
  definitions.
- **No async runtime opinions.** Building an instruction and decoding an account never pulls in Tokio or a
  Solana RPC client; those are pulled in only behind the `devnet-driver` feature, used by this crate's own
  example binaries (`examples/devnet_driver.rs`, `examples/find_max_batch_id.rs`) and by any caller that
  wants the same convenience.
- **`decode_batch_account` refuses a v1 batch account by name (`DecodeError::BadVersion`).** The batch
  account's header is versioned (currently 2, `rome-zk-layouts::batch`); `BatchAccount::open_unix_ts` is
  the committed `Clock::unix_timestamp` `OpenBatch` writes alongside `open_slot` — the anchor the
  derivation node's timestamp drift bound checks every block against. There is no migration: a caller
  that reads an old (v1) account gets a named error, never a silently zero-padded or misaligned decode.
- **`chunk_account_index`/`batch_account_index` are the single place a consumer of transaction history
  (the settlement watcher, tooling, the derivation node) looks up which `AccountMeta` slot in a decoded
  `InboxIx` carries the chunk/batch PDA** — pinned by this crate's own tests against every builder's real
  account list, so a builder's account order changing without these functions changing is caught here,
  not downstream.

## Config

None — this is a pure library crate. Callers supply their own program id, RPC endpoint and signing key; a
signing key is never read or handled by this crate itself.

## How to test

```sh
cargo test -p zk-inbox-client
```

Some of this crate's tests build real instructions against [`programs/zk-inbox`](../../programs/zk-inbox)
and check them through `solana-program-test`, so build the inbox program first: `cargo build-sbf
--manifest-path programs/zk-inbox/Cargo.toml --sbf-out-dir target/deploy`.

## Security notes

The off-chain `acc` reference implementation exists so a consumer of this crate — a batch poster deciding
whether to trust a batch account's claimed commitment, for instance — never has to take the on-chain
program's own computation on faith; it can recompute the same commitment from the same inputs using this
crate's software keccak and compare. This only holds because [`rome-zk-layouts`](../rome-zk-layouts) and
[`rome-zk-merkle`](../rome-zk-merkle) define the commitment formula once, shared by both the on-chain
program and this crate — see those crates' own READMEs for what makes that sharing airtight.

## Design references

The lane design covers the inbox chunk header and channel/frame format, and the batch account and its
accumulator — the instructions this crate builds against. See
[`programs/zk-inbox`](../../programs/zk-inbox)'s own README for the invariants the program enforces on
each of these instructions.

## Depends on

[`programs/zk-inbox`](../../programs/zk-inbox) itself (built with its `no-entrypoint` feature, so this
crate links against its instruction and account types without pulling in the Solana program entrypoint);
[`rome-zk-merkle`](../rome-zk-merkle) and [`rome-zk-layouts`](../rome-zk-layouts) for the shared Merkle
reduction and account layouts. Consumed by [`rome-zk-batcher`](../rome-zk-batcher) and by
[`zk-settlement-client`](../zk-settlement-client) (for the inbox-side PDA derivations the settlement
program's own instructions need to reference).
