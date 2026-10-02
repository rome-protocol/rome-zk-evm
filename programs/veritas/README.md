# Veritas

Rome's verifier for ZisK's PLONK proofs over BN254, available as a Rust library and a Solana program.
It was written clean-room from Rome's own functional specification of the proof system.

## Entry points

The library exposes two verification functions:

| Function | Input |
|---|---|
| `verify_zisk` | 1,344-byte ABI: 768-byte proof, 32-byte `programVK`, 32-byte `rootCVadcopFinal`, then 512-byte `publicValues` |
| `verify` | 800 bytes: 768-byte proof followed by a 32-byte public signal |

`verify_zisk` derives the signal from SHA-256 of `programVK || publicValues || rootCVadcopFinal`,
reduced modulo the BN254 scalar-field modulus. Both functions return `Ok(true)` for an accepted proof,
`Ok(false)` for a rejected proof, or `Err(ProgramError::InvalidInstructionData)` for malformed input.

The Solana program accepts only the 1,344-byte ABI and needs no accounts. It succeeds only on
`Ok(true)`; both REJECT and MALFORMED return `InvalidInstructionData`.

## What it guarantees

Veritas checks a proof against a public signal using its compiled-in verifying key. It does not bind
the proof to a block, batch or chain. Those checks belong to
[`zk-settlement`](../zk-settlement/README.md); see the
[trust model](../../docs/ARCHITECTURE.md#trust-model) for what each settlement layout binds.

`zk-settlement` links Veritas with `no-entrypoint` and calls `verify_zisk` inside `PostRootProved`.
The prover links the same library and checks each proof before posting it.

## How to test

From the repository root, on a build machine:

```sh
cargo build-sbf --arch v3 --manifest-path programs/veritas/Cargo.toml --sbf-out-dir target/deploy
cargo test -p veritas
cargo test -p veritas --test bpf -- --nocapture --test-threads=1
```

Build first: `tests/bpf.rs` loads `target/deploy/veritas.so` and runs real SBPF v3 bytecode in
`solana-program-test`. The block-14 proof measured **442,210 CU**; the compiled program was **55,480 bytes**.
The last command prints the CU measurements.

The source comments refer to a written specification of the verifier, by section number. The specification
is not part of this repository.
