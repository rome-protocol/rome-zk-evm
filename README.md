# rome-zk

Rome ZK lane: a proved EVM rollup that sequences transactions off-chain and settles, publishes every
transaction and verifies state-root proofs on Solana. Solana is the only settlement, data-availability
and verification layer this system relies on — there is no separate data-availability committee and no
other chain in the trust path. This repository contains four Solana programs (the inbox, settlement,
the PLONK verifier, and the bridge vault), plus off-chain sequencer, batcher, derivation, proving,
and settlement services. Most Rust crates share one Cargo workspace; the prover, its input tools and
the measurement guest have separate workspaces.

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
  The four on-chain programs pin `solana-program = "=4.1.0"` (the whole workspace's one Agave 4.3.0-line
  crate set, `Cargo.toml`'s `[workspace.dependencies]`) and build
  `--arch v3` — the CLI's bundled platform-tools (≥ v1.54) builds them independently of the host Rust
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
[operator runbook](deploy/rollup/README.md). Public devnet programs, published images and the guest build
for your chain id come later.
For the available services and their configuration, start with
[Deployment](docs/ARCHITECTURE.md#deployment) and the [component READMEs](docs/ARCHITECTURE.md#components).

## License

The public repository will use Apache-2.0. The workspace already declares it in `Cargo.toml`;
the root `LICENSE` file is still pending.
