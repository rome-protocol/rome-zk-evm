# rome-zk-channel

The channel/frame stream codec: `RLP([blocks]) -> zstd -> frames`, and the inverse, plus the shadow
compressor that decides when a channel is full. One codec, depended on directly by both the crate that
writes it and the crate that reads it, rather than reimplemented on either side.

## Why this crate exists

Before it did, the codec lived inside `rome-zk-batcher` (`src/channel.rs`), and `rome-zk-derive` reached
it only by depending on the whole batcher crate — its RPC clients, its sender, its Solana account
preflight, everything — just to call `decode_stream`/`reassemble` on a handful of bytes. Splitting it out
means the derivation node's dependency graph reflects what it actually needs: a pure, `no_std`-adjacent
codec with no Solana RPC client, no tokio runtime, no sender.

## What it owns

- **`Block`** — one block's channel-stream content: `number`, `timestamp`, `gas_limit`, and `txs` (raw,
  already-signed EIP-2718 tx bytes; this codec never parses tx contents), plus an optional fifth field,
  `deposits_end`, the deposit queue's index after the block. `None` encodes the same four-item list as
  before; `resolve_deposits_end` refuses a value that does not move past the previous block's.
- **`encode_stream`/`decode_stream`** — `RLP([Block]) -> zstd-19`, and the inverse. The one-shot (not
  streaming) codec used for the final encoding a channel closes with, and for decoding it back.
- **`channel_id`** — `keccak256(chain_id_le[8] ++ batch_le[8])[..16]`, binding every frame of one batch's
  channel stream to exactly that `(chain_id, batch)` pair.
- **`Frame`** and **`ChannelError`** — one frame (a 19-byte header from
  [`rome-zk-layouts::frame`](../rome-zk-layouts) plus a compressed body slice), its `to_bytes`/`from_bytes`
  wire encoding, and every way a frame or a set of frames can fail to parse or reassemble.
- **`cut_frames`/`reassemble`** — splits a compressed stream into frames of at most
  `DEFAULT_MAX_FRAME_BODY_LEN` (3,681) bytes each, and the inverse: reassembles an out-of-order,
  possibly-duplicated frame set (checking channel-id agreement, frame-number contiguity, and exactly one
  `is_last`) back into the compressed stream.
- **`ShadowCompressor`** — a streaming, size-aware channel builder (mirrors `op-batcher`'s
  `ChannelBuilder.AddBlock`): appends blocks one at a time, refusing (without mutating state) the block
  that would push the channel over its configured frame budget, so a channel is closed before it would
  ever need to emit an oversize frame.

The frame *header*'s byte layout (16-byte channel id, 2-byte frame number, 1-byte `is_last`) is not owned
here — it is a single definition in [`rome-zk-layouts::frame`](../rome-zk-layouts), consumed identically by
this crate, the on-chain programs, and every client.

## Consumers

- **`rome-zk-batcher`** re-exports this crate's whole public surface under its historical
  `rome_zk_batcher::channel` module path (`channel.rs` is now a one-line `pub use`), so no existing call
  site or test in that crate changed. It is the encoder: `pipeline.rs` builds `Block`s from the sequencer's
  ordered log, `grouping.rs`/`ShadowCompressor` decide batch boundaries, and the result is cut into frames
  and posted as inbox chunks.
- **`rome-zk-derive`** depends on this crate directly (not on the batcher) — it is the decoder:
  `frame_queue.rs` parses each chunk's raw bytes via `Frame::from_bytes`, `channel_bank.rs` buffers frames
  per channel and reassembles a complete one, and `batch_queue.rs` calls `decode_stream` to get back the
  ordered `Block` list it validates strictly.
- The indexer is a future third reader of the same wire bytes, for the same reason: reading a chunk's
  contents should never require depending on the batcher's sender or its Solana RPC surface.

## Wire facts

- Frame header: 19 bytes (`rome-zk-layouts::frame`). `reassemble` accepts frames in any order and refuses, by
  name, a foreign `channel_id`, a missing frame, a mis-placed `is_last`, and any duplicate `frame_no`.
- Default max compressed frame body: 3,681 bytes — chosen so one inbox chunk lane (`Open` + `Write` + `Seal`
  + `SealLeaf`) fits one V1 (SIMD-0385, 4,096-byte) transaction with room to spare (measured: 4,084 B;
  see `rome-zk-solana-sender`'s own pin test).
- Default max frames per channel before the shadow compressor closes it early: 900 (≈ 3.3 MiB compressed).

## Features

- `zstd-c` (default): the C `zstd` crate — `encode_stream` (level 19), `decode_stream`, `ShadowCompressor`.
  Every host consumer (batcher, derive, tests) uses this.
- `decode-pure`: `decode_stream_pure` via the pure-Rust `ruzstd` decoder — the path a zkVM guest takes.
  `cargo test --features decode-pure` runs the pure == C equivalence test on real encoder
  output; CI runs it as a required step. Building with `--no-default-features --features decode-pure`
  compiles without any C code.

This crate's own dependency on [`rome-zk-layouts`](../rome-zk-layouts) is `default-features = false`
— it only ever touches `frame::{FRAME_HEADER_LEN, write_header, read}`, never a
`Pubkey`, so it never needs layouts' `solana` feature. Combined with `--no-default-features --features
decode-pure` on this crate, that is what lets `rome-zk-channel` build for the ZisK guest target
(`riscv64ima-zisk-zkvm-elf`, no `solana-program`/`getrandom` in the graph) — see
[`guest/rome-zk-bench-decode`](../../guest/rome-zk-bench-decode), which depends on this crate exactly that
way. `cargo check -p rome-zk-channel --no-default-features --features decode-pure` is a required CI check.
