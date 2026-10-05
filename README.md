# rome-zk

Rome ZK lane: a proved EVM rollup that sequences transactions off-chain and settles, publishes every
transaction and verifies state-root proofs on Solana. Solana is the only settlement, data-availability
and verification layer this system relies on — there is no separate data-availability committee and no
other chain in the trust path. This repository contains four Solana programs (the inbox, settlement,
the PLONK verifier, and the bridge vault), plus off-chain sequencer, batcher, derivation, proving,
and settlement services. Most Rust crates share one Cargo workspace; the prover, its input tools and
the measurement guest have separate workspaces.

On Solana devnet, anyone can register a chain and request its verification key through the
[issue form](https://github.com/rome-protocol/solana-zk-evm/issues/new?template=vkey-request.yml).
Once Rome registers the key, the operator can run the prover and post proved roots, which are final on Solana. Deposits and withdrawals work end to end on devnet too: SOL deposited on Solana is credited on the chain, and a withdrawal started on the chain is proved and paid out on Solana by the exit prover, without anyone stepping in. The node image is
`ghcr.io/rome-protocol/rome-zk-evm:v0.3.0`; the guest is `rome-zk-guest` `v0.3.0`, using ZisK
`1.3.1-alpha`. The prover leaves old pending accounts open by default, and nothing in this release closes inbox batches, and Rome has not yet decided how long old batch data must stay on Solana. The devnet programs can be upgraded without notice.

- **Architecture:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — components, the transaction data flow, the
  trust model, and the threat model. Start with its
  [Components at a glance](docs/ARCHITECTURE.md#components-at-a-glance): a data-flow diagram and a compact
  summary, followed by a table of the core components, their code and who runs them.
- **Wire formats and limits:** [Data formats](docs/ARCHITECTURE.md#data-formats) and [`crates/rome-zk-layouts`](crates/rome-zk-layouts) — the transaction format, byte limits and
  batch shape every client and program must agree on.
- **Changes:** [`CHANGELOG.md`](CHANGELOG.md).
- Every crate and program directory has its own `README.md` covering its purpose, invariants, configuration,
  and exact test commands.

## Layout

```
programs/    Solana on-chain programs: zk-inbox (data availability + Merkle accumulator),
             zk-settlement (chains, root posting, finality, exits, fees),
             veritas (the PLONK proof verifier), zk-bridge (the vault)
contracts/   exit-portal, the Solidity contract on the rollup where a withdrawal starts
guest/       rome-zk-bench-decode, a measurement guest used for sizing; not production code
crates/      off-chain services and shared libraries — see each crate's own README for its role
fixtures/    proof and calldata fixtures the program tests load — never regenerate silently; they pin
             verifying keys the tests check against
migrations/  Postgres (sqlx) migrations shared by the off-chain services
```

The production guest lives in `rome-protocol/rome-zk-guest`, under `crates/clients/rome/guest`.
Veritas ([`programs/veritas`](programs/veritas/README.md)) is Rome's own verifier for ZisK's PLONK proofs,
written from Rome's own functional specification of the proof system. zk-settlement links it as a library
and runs it inside `PostRootProved`; the prover runs the same check off-chain before posting.

## Prerequisites

- Rust, pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (`rustup` picks it up automatically).
- The Agave (Solana) CLI, for `cargo build-sbf` — install via `sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"`.
  The four on-chain programs pin `solana-program = "=4.1.0"`, set once for the whole workspace in `Cargo.toml`, and
  build with `--arch v3`. The CLI's bundled platform tools (v1.54 or newer) build them, independent of the host Rust
  toolchain above.
- Docker, only if building the sequencer's container image (see [`Dockerfile`](Dockerfile)).

## Build and test

The integration tests also need the test-only bridge binary described in
[`zk-settlement`](programs/zk-settlement/README.md#how-to-test) and the SPL Token and Associated Token
Account binaries described in [`zk-bridge`](programs/zk-bridge/README.md). Prepare those fixtures before
running `cargo test --workspace`.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
for p in programs/*/; do cargo build-sbf --arch v3 --manifest-path "$p/Cargo.toml" --sbf-out-dir target/deploy; done
cargo test --workspace
```

Build the on-chain programs *before* running the test suite: several program-test suites load the real
compiled `.so` from `target/deploy/` so their compute-unit assertions measure real BPF execution, not the
native-processor path program-test would otherwise fall back to. Each crate and program README also gives
the narrower command for testing just that one package. The root commands do not test the separate
prover, input-generator, cross-repository wire-test or measurement-guest workspaces; use their READMEs
for those commands.

## Run your own rollup

The portable [`deploy/rollup/`](deploy/rollup/) directory is available, with setup and commands in its
[operator runbook](deploy/rollup/README.md).
[Run on Solana devnet](docs/RUN-ON-DEVNET.md) walks through running your own chain on Rome's shared settlement.
[Devnet trust model](docs/TRUST-MODEL.md) explains who controls the shared programs and what they enforce.
[Monitoring one rollup](docs/MONITORING.md) covers health checks and example alerts for that chain.
[Prover host](docs/PROVER-HOST.md) sets up the GPU machine that proves your chain's batches.
[Withdrawals](docs/WITHDRAWALS.md) explains how the exit prover pays users from the chain's vault.
For the available services and their configuration, start with
[Deployment](docs/ARCHITECTURE.md#deployment) and the [component READMEs](docs/ARCHITECTURE.md#components).

## Support

Ask questions in [Discussions](https://github.com/rome-protocol/solana-zk-evm/discussions). Report a bug, or ask for
your chain's verification key, with the [issue forms](https://github.com/rome-protocol/solana-zk-evm/issues/new/choose).
For a security problem, follow [`SECURITY.md`](SECURITY.md) and do not open an issue.

## License

Copyright © 2024-2026 Coin Vesting Inc. d/b/a Rome Protocol. All rights reserved. You may use this software for
personal, non-commercial purposes; commercial use needs Rome Protocol's written permission. See [`LICENSE`](LICENSE),
and [`NOTICE`](NOTICE) for the parts under other licences. Releases up to v0.2.1 were published under Apache-2.0 and
keep that licence.

To report a security problem, see [`SECURITY.md`](SECURITY.md).
