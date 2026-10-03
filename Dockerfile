# Multi-stage build for one image carrying four rome-zk binaries — `rome-zk-sequencer` (the drop-in replacement
# for the stock `ghcr.io/paradigmxyz/reth:v2.5.2` container on Tiber, its `reth` service: same ports — 8545 http,
# 8546 ws, 9001 metrics — same `--http.api eth,net,web3,debug,txpool,trace,ots` surface, served by an in-process
# reth via `--executor reth`), `rome-zk-batcher` and `rome-zk-derive` (Tiber's settlement services), and `rome-zk-ops` (the operator CLI:
# settlement and bridge commands, a dry run unless `--confirm`). ENTRYPOINT stays the sequencer — the
# settlement-services compose file overrides `entrypoint:` to select the others out of the same image.
#
# Scoped to `-p rome-zk-sequencer -p rome-zk-batcher -p rome-zk-derive -p rome-zk-ops` (not the whole
# workspace): `programs/*` are Solana BPF crates built with `cargo build-sbf`'s own bundled platform-tools
# toolchain, not this image's rustc, and are irrelevant to these four binaries.

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

# One cargo invocation, four bins (production build; the per-crate lib-only CI step — .github/
# workflows/ci.yml's `clippy` job — already guards each crate's own feature masking standalone).
#
# Built with the `release-host` profile (Cargo.toml: thin LTO, 16 codegen units), not `release`: the workspace
# `release` profile is fat LTO with one codegen unit because the Solana programs need it, and linking the sequencer
# that way takes about 8 minutes. `release` itself is untouched, so the programs build exactly as before.
#
# The Cargo registry, the git checkouts and `target` live in BuildKit cache mounts, so the dependency build (reth and
# the rest) is kept on the build host between builds and only the workspace crates are rebuilt. A cache mount is not
# part of the image layer, so the four binaries are copied out of it, inside the same RUN, to /out. `target` is
# mounted with `sharing=locked` so two builds on the same host never write it at once.
#
# Cargo decides whether a workspace crate is up to date by file mtime alone. `target` is now shared by every build on
# the host, and COPY keeps the mtimes the files had in the build context (a checkout leaves an unchanged file with an
# old mtime). So a source file could be older than the cached build output while holding different content, and cargo
# would reuse the stale output and ship an image that does not match the commit. The `find ... touch` below sets every
# source file's mtime to now, before cargo runs, so the workspace crates always rebuild; the registry and git
# dependencies live outside the workspace and stay cached. The RUN layer then depends only on the content of the COPY
# layer above it, which is what makes reusing that layer from BuildKit's cache correct.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,sharing=locked \
    find . -path ./target -prune -o -type f -exec touch {} + \
    && cargo build --profile release-host --locked \
        -p rome-zk-sequencer -p rome-zk-batcher -p rome-zk-derive -p rome-zk-ops --bins \
    && mkdir -p /out \
    && cp target/release-host/rome-zk-sequencer target/release-host/rome-zk-batcher target/release-host/rome-zk-derive target/release-host/rome-zk-ops /out/

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 999 rome \
    && useradd --system --uid 999 --gid 999 --create-home --home-dir /home/rome --shell /usr/sbin/nologin rome \
    && mkdir -p /data/reth && chown rome:rome /data /data/reth

# rome-zk-derive's Cargo.toml pins `[[bin]] name = "rome-zk-derive"` (path `src/bin/rome_zk_derive.rs`)
# — the binary on disk is `rome-zk-derive`, never the source file's underscored `rome_zk_derive` name.
COPY --from=builder /out/rome-zk-sequencer /usr/local/bin/rome-zk-sequencer
COPY --from=builder /out/rome-zk-batcher /usr/local/bin/rome-zk-batcher
COPY --from=builder /out/rome-zk-derive /usr/local/bin/rome-zk-derive
COPY --from=builder /out/rome-zk-ops /usr/local/bin/rome-zk-ops

# The runtime user is uid/gid 999, set explicitly above and here: key files on the hosts are owned by uid 999.
USER 999:999
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
