# Multi-stage build for one image carrying all three rome-zk binaries — `rome-zk-sequencer` (the drop-in replacement
# for the stock `ghcr.io/paradigmxyz/reth:v2.5.2` container on Tiber, its `reth` service: same ports — 8545 http,
# 8546 ws, 9001 metrics — same `--http.api eth,net,web3,debug,txpool,trace,ots` surface, served by an in-process
# reth via `--executor reth`), `rome-zk-batcher` and `rome-zk-derive` (Tiber's settlement services). ENTRYPOINT
# stays the sequencer — the settlement-services compose file overrides `entrypoint:` to select the other two
# binaries out of the same image.
#
# Scoped to `-p rome-zk-sequencer -p rome-zk-batcher -p rome-zk-derive` (not the whole workspace):
# `programs/*` are Solana BPF crates built with `cargo build-sbf`'s own bundled platform-tools
# toolchain, not this image's rustc, and are irrelevant to these three binaries.

# Kept in lockstep with rust-toolchain.toml by hand (this image does not read that file — it IS the
# toolchain): bumped to 1.97.1 for `solana-client = 4.3.0`'s own `rust-version` floor.
FROM rust:1.97.1-bookworm AS builder
WORKDIR /build

# reth's own dependency graph needs: clang/libclang (bindgen, for rocksdb's C++ bindings),
# cmake + a C++ toolchain (rocksdb, mdbx), pkg-config + libssl-dev (openssl-sys transitively).
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake pkg-config libssl-dev build-essential git \
    && rm -rf /var/lib/apt/lists/*

COPY rust-toolchain.toml ./
RUN rustup show active-toolchain || rustup toolchain install

# Copy the whole workspace (path deps: rome-zk-executor-api, rome-zk-executor-reth); Cargo.lock is
# committed and must be respected (reth pins by tag, resolved once, never re-resolved here).
COPY . .

# One cargo invocation, three bins (production build; the per-crate lib-only CI step — .github/
# workflows/ci.yml's `clippy` job — already guards each crate's own feature masking standalone).
RUN cargo build --release --locked \
    -p rome-zk-sequencer -p rome-zk-batcher -p rome-zk-derive --bins

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/rome --shell /usr/sbin/nologin rome \
    && mkdir -p /data && chown rome:rome /data

# rome-zk-derive's Cargo.toml pins `[[bin]] name = "rome-zk-derive"` (path `src/bin/rome_zk_derive.rs`)
# — the binary on disk is `rome-zk-derive`, never the source file's underscored `rome_zk_derive` name.
COPY --from=builder /build/target/release/rome-zk-sequencer /usr/local/bin/rome-zk-sequencer
COPY --from=builder /build/target/release/rome-zk-batcher /usr/local/bin/rome-zk-batcher
COPY --from=builder /build/target/release/rome-zk-derive /usr/local/bin/rome-zk-derive

USER rome
WORKDIR /home/rome
VOLUME ["/data"]

EXPOSE 8545 8546 9001

# eth_chainId is the cheapest real end-to-end check: RPC server up, chain configured, executor
# answering — matches the contract's own healthcheck ask.
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=5 \
  CMD curl -sf -X POST -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
      http://127.0.0.1:8545 || exit 1

ENTRYPOINT ["/usr/local/bin/rome-zk-sequencer"]
CMD ["--config", "/data/config.toml", "--executor", "reth"]
