# The guest-build image: builds the batch guest for your chain's genesis from the public sources, with the ZisK 1.2.0-alpha
# toolchain. ./rollup guest-build builds and runs it for you; to do it by hand, from this directory:
#
#   docker build --platform linux/amd64 -t rome-zk-guest-build:local \
#     --build-arg ROME_ZK_EVM_TAG=v0.2.2 --build-arg ROME_ZK_GUEST_TAG=v0.2.0 .
#
# The image holds the toolchain and both source trees, and none of the ZisK proving keys. The two source tags are build
# arguments: the node's tag (ROME_ZK_EVM_TAG, the same tag your node image runs) and the guest's (ROME_ZK_GUEST_TAG).
# The guest is cloned as .fork/ under the node's repository, which is the layout its build script needs.
#
# The same sources and the same toolchain give the same ELF byte for byte, on any machine: the guest's build script
# builds from a fixed staging path and pins every crate to its lock file. The compiler is pinned too: ZisK's own Rust
# toolchain is a fixed tag (ZISK_TOOLCHAIN_TAG), and both ZisK downloads are checked against a sha256 below. The Ubuntu
# packages are the only thing not pinned to a hash. They provide compilers for the build scripts, which never reach the
# ELF. The rustup installer only manages the toolchain link and is not part of what the ELF is built from.
FROM ubuntu@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401

ARG ZISK_VERSION=1.2.0-alpha
# sha256 of the ziskup installer script of that release
ARG ZISKUP_SHA256=7473ab9181d69727838ed45e129c045e8016038b5bd2963dbfabc23a382d5a43
# ZisK's Rust toolchain: the tag, and the sha256 of its rust-toolchain-x86_64-unknown-linux-gnu.tar.gz
ARG ZISK_TOOLCHAIN_TAG=zisk-3.0.0
ARG ZISK_TOOLCHAIN_SHA256=74190d265b2d8f10424cdf666b71a5b5670c3a685785cd668ee5f49318fa149e
# sha256 of the cargo_zisk_linux_amd64.tar.gz release tarball that ziskup downloads
ARG CARGO_ZISK_SHA256=e3605dafb5fcdb2fa8271ea945334694f42704737f31ef0bdf93d663f2b14866
ARG ROME_ZK_EVM_TAG=v0.2.2
ARG ROME_ZK_GUEST_TAG=v0.2.0

ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/root \
    PATH=/root/.cargo/bin:/root/.zisk/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Compilers for the build scripts, the tools build-elf.sh calls (rsync, python3, strings, git) and what ZisK's binaries
# load (OpenMPI, OpenMP, GMP, libsodium).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git rsync python3 jq xz-utils tar binutils \
      build-essential pkg-config libssl-dev clang libclang-dev cmake \
      libomp-dev libgmp-dev libsodium-dev libopenmpi-dev openmpi-bin openmpi-common \
    && rm -rf /var/lib/apt/lists/*

# rustup with no default toolchain: ziskup asks cargo-zisk to install ZisK's own Rust toolchain through it.
RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none

# ZisK itself, the CPU build, with no proving keys (those are about 26 GB and only the programVK step needs them).
#
# Both downloads are pinned by hash. The toolchain tarball is fetched here, checked, and handed to ziskup through
# ZISK_TOOLCHAIN_SOURCE_DIR, so ziskup installs exactly that archive and never looks up the newest toolchain tag itself.
# ziskup downloads the cargo_zisk tarball on its own and has no hash check, so the same tarball is fetched and checked
# here as well, and every binary ziskup installed must be byte for byte the one in it.
RUN set -eu; \
    mkdir -p /tmp/zisk-pin/toolchain /tmp/zisk-pin/bundle; \
    curl -fsSL -o /tmp/zisk-pin/toolchain/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz \
      "https://github.com/0xPolygonHermez/rust/releases/download/${ZISK_TOOLCHAIN_TAG}/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz"; \
    echo "${ZISK_TOOLCHAIN_SHA256}  /tmp/zisk-pin/toolchain/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz" | sha256sum -c -; \
    curl -fsSL -o /tmp/zisk-pin/cargo_zisk.tar.gz \
      "https://github.com/0xPolygonHermez/zisk/releases/download/v${ZISK_VERSION}/cargo_zisk_linux_amd64.tar.gz"; \
    echo "${CARGO_ZISK_SHA256}  /tmp/zisk-pin/cargo_zisk.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/zisk-pin/cargo_zisk.tar.gz -C /tmp/zisk-pin/bundle ./bin; \
    curl -fsSL -o /usr/local/bin/ziskup "https://raw.githubusercontent.com/0xPolygonHermez/zisk/v${ZISK_VERSION}/ziskup/ziskup"; \
    echo "${ZISKUP_SHA256}  /usr/local/bin/ziskup" | sha256sum -c -; \
    chmod +x /usr/local/bin/ziskup; \
    ZISK_TOOLCHAIN_SOURCE_DIR=/tmp/zisk-pin/toolchain ziskup -v "${ZISK_VERSION}" --cpu --nokey -y --prefix /root/.zisk; \
    cargo-zisk --version | grep -F " ${ZISK_VERSION} "; \
    rustup toolchain list | grep -q '^zisk'; \
    for f in /tmp/zisk-pin/bundle/bin/*; do \
      n=$(basename "$f"); \
      case "$n" in ziskup|*-gpu) continue ;; esac; \
      i="${n%-cpu}"; \
      cmp "$f" "/root/.zisk/bin/$i"; \
    done; \
    rm -rf /root/.zisk/cache/* /tmp/*

# The two public source trees. Submodules come with the guest.
WORKDIR /work
RUN git clone -q --branch "${ROME_ZK_EVM_TAG}" --depth 1 https://github.com/rome-protocol/solana-zk-evm.git rome-zk-evm \
    && git clone -q --branch "${ROME_ZK_GUEST_TAG}" --depth 1 https://github.com/rome-protocol/rome-zk-guest.git rome-zk-evm/.fork \
    && git -C rome-zk-evm/.fork submodule update --init --recursive -q \
    && git -C rome-zk-evm rev-parse HEAD > /work/rome-zk-evm.rev \
    && git -C rome-zk-evm/.fork rev-parse HEAD > /work/rome-zk-guest.rev

ENV ROME_ZK_EVM_TAG=${ROME_ZK_EVM_TAG} ROME_ZK_GUEST_TAG=${ROME_ZK_GUEST_TAG}
COPY guest-build.sh /usr/local/bin/guest-build
RUN chmod +x /usr/local/bin/guest-build
LABEL org.opencontainers.image.title="rome-zk-guest-build" \
      org.opencontainers.image.description="Builds the Rome batch guest for one genesis with ZisK ${ZISK_VERSION}" \
      zisk.version="${ZISK_VERSION}" zisk.toolchain="${ZISK_TOOLCHAIN_TAG}" rome-zk-evm.tag="${ROME_ZK_EVM_TAG}" rome-zk-guest.tag="${ROME_ZK_GUEST_TAG}"

# /in/genesis.json is the genesis (read-only), /out receives the ELF and vkey.json, /keys/provingKey is the optional
# ZisK proving key directory (read-only). See guest-build.sh.
ENTRYPOINT ["/usr/local/bin/guest-build"]
