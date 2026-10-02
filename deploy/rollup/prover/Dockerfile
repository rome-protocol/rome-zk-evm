# The prover image for a rollup node. Build it from the repository root:
#
#   docker build -f deploy/rollup/prover/Dockerfile -t rome-zk-prover:local .
#
# (`./rollup up` with PROVER=on builds it for you the first time: the compose file carries this build.)
#
# The prover is its own cargo workspace under crates/rome-zk-prover, so it builds from that directory rather than from
# the repository root's workspace.
#
# The image does not contain the ZisK toolchain or the proving keys. Download them once on the host (about 77 GB)
# and mount the directory at /opt/zisk; the compose file does that from ZISK_HOME. What the image does carry is what
# the prover needs around them: the CUDA and OpenMPI runtime libraries, and Node with snarkjs, which the PLONK step
# calls.
FROM rust:1.97.1-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake pkg-config libssl-dev build-essential git \
    && rm -rf /var/lib/apt/lists/*

COPY rust-toolchain.toml ./
RUN rustup show active-toolchain || rustup toolchain install

COPY . .
WORKDIR /build/crates/rome-zk-prover
RUN cargo build --release --locked --bin rome-zk-prover

FROM nvidia/cuda:12.9.0-runtime-ubuntu22.04 AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl openmpi-bin libopenmpi3 gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g snarkjs@0.7.6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/rome --uid 999 --shell /usr/sbin/nologin rome \
    # /opt/zisk/cache is created here so the named volume the compose file mounts over it starts out owned by uid 999.
    && mkdir -p /data /data/work /opt/zisk /opt/zisk/cache /run/rome-zk && chown -R rome:rome /data /opt/zisk

COPY --from=builder /build/crates/rome-zk-prover/target/release/rome-zk-prover /usr/local/bin/rome-zk-prover
COPY deploy/rollup/prover/entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

USER rome
WORKDIR /home/rome
VOLUME ["/data/work"]

EXPOSE 9004

# The prover's /metrics answers while the process is alive, including when it is idle.
HEALTHCHECK --interval=15s --timeout=3s --start-period=30s --retries=5 \
  CMD curl -sf http://127.0.0.1:9004/metrics >/dev/null || exit 1

# The entrypoint checks the proving keys against the manifest mounted at /opt/prover on every start, then runs the prover.
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["--config", "/data/prover.toml", "--follow"]
