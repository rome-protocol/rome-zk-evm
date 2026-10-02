# zk-exit-stub-bridge

A minimal, TEST-ONLY stand-in for the real bridge program (`programs/zk-bridge`) — never
shipped as a deployed program. It exists solely so `programs/zk-settlement`'s real-BPF
`ConsumeExit` tests (`programs/zk-settlement/tests/exit_consume.rs`) can exercise a genuine CPI-signed
`exit_consumer` PDA, the same way `programs/zk-settlement`'s own `tests/settlement.rs` already loads
`zk-inbox` as a second real program in one `solana-program-test` run.

## What it does

One instruction: decode `(chain_id: u64 LE, message_hash: [u8; 32])` from its own instruction data, then
CPI `programs/zk-settlement`'s `ConsumeExit` (discriminant 29), signing the `exit_consumer` PDA
`["exit_consumer", chain_id]` **under its own program id** via `invoke_signed` — the one thing only a
program's own runtime identity can do, never a keypair (a PDA has no private key). This is exactly the
mechanism the real `zk-bridge` program uses, and exactly what `ConsumeExit`'s `NotBridgeProgram` guard
tests: whether the signed `exit_consumer` PDA equals `exit_config.bridge_program`'s own derivation.

Accounts (this program's own instruction, in order): `[settlement_program (read-only, executable),
bridge_signer (the exit_consumer PDA under THIS program's id), exit_config (read-only), exit_record
(writable), payer_refund (writable)]` — the same four accounts `ConsumeExit` itself reads, passed straight
through in the CPI.

## Why its own workspace

This crate lives two directories below `programs/zk-settlement/tests/`, outside the root workspace's
`programs/*` member glob, and declares its own `[workspace]` table so it is never accidentally inferred as
part of the root workspace. The CI `test` job builds its `.so` by explicit manifest path — never through
the `programs/*/` loop the `build-sbf` job's release-artifact upload uses — so it can never be mistaken
for, or accidentally published alongside, a real deployed program.

## How to build

```sh
cargo build-sbf --manifest-path programs/zk-settlement/tests/fixtures/stub-bridge/Cargo.toml --sbf-out-dir target/deploy
```

Run before `cargo test -p zk-settlement --test exit_consume` (the `.so` is loaded from `target/deploy` by
`rome_zk_testkit::program_test`, exactly like every other program this workspace's tests load).
