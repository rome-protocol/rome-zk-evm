# rome-zk-bench-decode — measurement guest

A small ZisK guest that measures what the real guest will pay for the DA side of a batch:
keccak per chunk body → indexed-leaf Merkle root → `acc` (the inbox commitment), frame reassembly,
pure-Rust zstd decode (`ruzstd`), RLP block decode. It is **not** the production guest.

Its own cargo workspace (excluded from the repo root): `lib/` holds the measured logic (host-testable,
no `ziskos`), `bin/` is the thin `ziskos::entrypoint!` over it.

## Real crates, no shim

This guest used to carry a `shim` module that duplicated the 19-byte frame header and the
`acc`/`forced_empty_root` formulas, because `rome-zk-layouts` depended on `solana-program`
unconditionally (every module's `pda()` needed `Pubkey`), which drags in `getrandom 0.1.16` — no
`riscv64ima-zisk-zkvm-elf` backend — and `rome-zk-channel` depended on layouts, so it was equally blocked.
That dependency is now gated behind a default-on `solana` feature on
[`rome-zk-layouts`](../../crates/rome-zk-layouts), and [`rome-zk-channel`](../../crates/rome-zk-channel)
depends on it with `default-features = false` — both now build for the ZisK target with no `solana-program`
in the graph. The shim is deleted: `lib/src/lib.rs`'s stages call `rome_zk_layouts::{forced_empty_root,
acc}` and `rome_zk_channel::{Frame::from_bytes, reassemble, decompress_pure}` directly — one definition of
each fact, in the crate that owns it.

## Build the guest ELF (on a build machine only — never a local workstation)

```sh
cd guest/rome-zk-bench-decode/bin        # bin/.cargo/config.toml's rustflags apply only from here
~/.zisk/bin/cargo-zisk build --release                                              # full pipeline
~/.zisk/bin/cargo-zisk build --release --no-default-features                        # keccak+merkle+acc only
~/.zisk/bin/cargo-zisk build --release --no-default-features --features stage_ruzstd  # + reassemble/ruzstd
# ELF: bin/target/elf/riscv64ima-zisk-zkvm-elf/release/rome-zk-bench-decode
```

Stage step counts are the differences between the three builds' `ziskemu -m` totals. On the real crates
the full pipeline measures 36,968 steps on the real batch 2043 (previously 40,872 on the shim) and
16,269,466 on the 10 × 300 synthetic batch (previously 16,247,685, +0.13 %). The real batch's ~9.5 % drop
is a fixed cost, not a decode-cost change: the guest used to commit `keccak(acc ‖ block_count)` (one extra
hash call over the ~40 committed bytes); `run` now returns `(block_count, acc)` directly and `bin/`'s
entrypoint commits both as raw slices (see "Run" below), so that extra hash never runs. The synthetic
batch's step count — two orders of magnitude larger, so a fixed per-run cost barely moves it — staying
within 0.13 % confirms the per-byte decode/hash cost this guest measures is unchanged.

## Inputs

```sh
cd guest/rome-zk-bench-decode
cargo run -p rome-zk-bench-decode-lib --bin gen_input -- synthetic --blocks 10 --txs-per-block 300 --tx-len 110 --out inputs/syn-300.bin
cargo run -p rome-zk-bench-decode-lib --bin gen_input -- from-fixture --fixture ../../fixtures/inbox/txv1-dev-batch-2043.json --out inputs/real-2043.bin
cargo run -p rome-zk-bench-decode-lib --bin fetch_real_batch -- --help    # read-only fetch of a finalized batch's chunk accounts from the dev chain; see its flags
```

`gen_input` uses the real batcher encoder (`rome_zk_channel::encode_stream` level 19 + `cut_frames`), so
synthetic inputs have real frame counts; `fetch_real_batch` reads a finalized batch's chunk accounts and
records the on-chain `acc`, which the guest's recomputed `acc` is asserted against — both host-side
(`stage_hash_reproduces_the_real_fixtures_on_chain_acc`, below) and at the ELF level (`ziskemu`'s committed
output, next section).

## Run

```sh
~/.zisk/bin/ziskemu -e bin/target/elf/riscv64ima-zisk-zkvm-elf/release/rome-zk-bench-decode -i inputs/syn-300.bin -m   # steps + wall
~/.zisk/bin/ziskemu -e ... -i inputs/real-2043.bin -o commits.bin                                                     # the committed output
```

`bin/`'s entrypoint commits `acc` (32 bytes) then `block_count` (8 bytes, LE) as two raw slices — never
mixed into one hash — so `commits.bin`'s first 32 bytes are `acc` in the clear: on the committed real-batch
fixture that is `965a19bccd2c6b4f674b9201f157b5a96cc1053b1041d26a361b3d70431dbbf3`, matching the fixture's
recorded on-chain value byte-for-byte.

## Tests

```sh
cd guest/rome-zk-bench-decode && cargo test --locked -p rome-zk-bench-decode-lib
```

The lib tests prove: the pipeline equals `rome_zk_channel::decode_stream` (C) on real encoder output;
`stage_hash`'s `acc` over the committed real-batch fixture equals the fixture's recorded on-chain `acc`
(`stage_hash_reproduces_the_real_fixtures_on_chain_acc`) and stops matching if one chunk-body byte is
flipped (`corrupting_the_real_fixtures_chunk_body_breaks_the_acc_match`, refused by name — "acc mismatch");
frames reassemble in any order; a foreign channel_id or a missing frame is refused by name; a corrupted
body panics by name. CI runs them.

`rome_zk_channel::reassemble` refuses a repeated frame number by name (`ChannelError::DuplicateFrame`),
whether or not the bytes match. The shim this crate carried before the switch to the real crates had a
duplicate check of its own; the guest now relies on the channel crate's.
