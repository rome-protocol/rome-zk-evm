# Veritas

Rome's verifier for ZisK's PLONK proofs over BN254, available as a Rust library and a Solana program.
It was written clean-room from Rome's own functional specification of the proof system.

## Entry points

The library exposes two verification functions:

| Function | Input |
|---|---|
| `verify_zisk(version, abi)` | a ZisK release row, and the 1,344-byte ABI: 768-byte proof, 32-byte `programVK`, 32-byte `rootCVadcopFinal`, then 512-byte `publicValues` |
| `verify(key, data)` | a wrapper key, and 800 bytes: 768-byte proof followed by a 32-byte public signal |

`verify_zisk` derives the signal from SHA-256 of `programVK || publicValues || rootCVadcopFinal`,
reduced modulo the BN254 scalar-field modulus. Both functions return `Ok(true)` for an accepted proof,
`Ok(false)` for a rejected proof, or `Err(ProgramError::InvalidInstructionData)` for malformed input.

## ZisK releases

Each ZisK release has its own wrapper key and its own recursion root (`rootCVadcopFinal`). The release table
in `src/versions.rs` has one row per release, numbered as the settlement registry's `scheme` byte, and
`zisk_version(scheme)` finds a row. Numbers are never reused.

| `scheme` | Release | Status | Key |
|---|---|---|---|
| 1 | ZisK 1.2.0-alpha | Withdrawn | only behind the `zisk-1-2-0-test-key` feature |
| 2 | ZisK 1.3.1-alpha | Open | compiled in |

`verify_zisk` rejects a proof whose `rootCVadcopFinal` is not the row's pinned value before it hashes anything,
and a row with no key cannot verify (`InvalidArgument`). The release comes from the caller, never from the
proof. Adding a release is a new row, with its eight commitments read from ZisK's `PlonkVerifier.sol` and its
root from `ZiskVerifier.getRootCVadcopFinal()` at the release tag, and a program upgrade.

The Solana program takes one release byte (the scheme number) followed by the 1,344-byte ABI, and needs no
accounts. It succeeds only on `Ok(true)` under that release; REJECT, MALFORMED, an unknown release byte and a
withdrawn release all return `InvalidInstructionData`.

## What it guarantees

Veritas checks a proof against a public signal using its compiled-in verifying key. It does not bind
the proof to a block, batch or chain. Those checks belong to
[`zk-settlement`](../zk-settlement/README.md); see the
[trust model](../../docs/ARCHITECTURE.md#trust-model) for what each settlement layout binds.

`zk-settlement` links Veritas with `no-entrypoint` and calls `verify_zisk` inside `PostRootProved`, with the
release row of the registry entry. Through settlement, a real ZisK 1.3.1 layout-1 batch proof posts in 468,334 to 490,834
compute units (measured on real BPF), which includes this verification. The range comes from the bump search
for the program's accounts, which depends on the program id and the chain id, not on the proof; the
verification itself measures 437,919 to 442,234.
The prover links the same library and checks each proof before posting it.

The standalone program is deployed on Solana devnet at `2cLGd9FKC7TiZrEHCvpw291AwXNcGgP9nDT4W3PHLe5k`,
with the other shared programs in [`deploy/rollup/programs.devnet.json`](../../deploy/rollup/programs.devnet.json).
Settlement does not call that program: it runs the same code linked into itself.

## How to test

From the repository root:

```sh
cargo build-sbf --arch v3 --manifest-path programs/veritas/Cargo.toml --sbf-out-dir target/deploy
cargo test -p veritas
cargo test -p veritas --test bpf -- --nocapture --test-threads=1
```

Build first: `tests/bpf.rs` loads `target/deploy/veritas.so` and runs real SBPF v3 bytecode in
`solana-program-test`. The built program holds the ZisK 1.3.1 key only, so it runs the 1.3.1 proofs. The
block-14 proof measured **437,919 CU**; the compiled program was **56,112 bytes**. The last command prints the
CU measurements. The library tests turn on the `zisk-1-2-0-test-key` feature themselves, so they also check each
release's proofs against the other release's row.

The source comments refer to a written specification of the verifier, by section number. The specification
is not part of this repository.
