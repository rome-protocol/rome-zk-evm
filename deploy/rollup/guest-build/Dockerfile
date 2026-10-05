# The guest-build image: builds the batch guest for your chain's genesis from the public sources, with the toolchain of one
# ZisK release. ./rollup guest-build builds and runs it for you; to do it by hand, from this directory:
#
#   docker build --platform linux/amd64 -t rome-zk-guest-build:local \
#     --build-arg ROME_ZK_EVM_TAG=v0.3.0 --build-arg ROME_ZK_GUEST_TAG=v0.3.0 .
#
# The image holds the toolchain and both source trees, and none of the ZisK proving keys. The ZisK release is a build
# argument (ZISK_RELEASE); everything that belongs to a release is pinned in zisk/<release>.env: the download URLs with
# their sha256 values, the Rust toolchain tag and the vadcop_final root of its key set. For a release other than the
# default, pass its toolchain tag as well (ZISK_TOOLCHAIN_TAG); it is only here so the image can carry it as a label, and
# the build stops unless it is the pin file's. The two source tags are build arguments too: the node's tag
# (ROME_ZK_EVM_TAG, the same tag your node image runs) and the guest's (ROME_ZK_GUEST_TAG). The image's labels record all
# four values, and files in the image record the release and the two tags for guest-build, so the environment of a
# container cannot change what vkey.json says.
# The guest is cloned as .fork/ under the node's repository, which is the layout its build script needs. Its Cargo.lock
# must pin ziskos and every other zisk-* crate at the image's release; a guest locked to another release is refused.
#
# The same sources and the same toolchain give the same ELF byte for byte, on any machine: the guest's build script
# builds from a fixed staging path and pins every crate to its lock file. The compiler is pinned too: ZisK's own Rust
# toolchain is a fixed tag (ZISK_TOOLCHAIN_TAG in the pin file), and every ZisK download is checked against its sha256. The Ubuntu
# packages are the only thing not pinned to a hash. They provide compilers for the build scripts, which never reach the
# ELF. The rustup installer only manages the toolchain link and is not part of what the ELF is built from.
FROM ubuntu@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401

# The ZisK release to install. Its pins are in zisk/${ZISK_RELEASE}.env. The release is the default until a build names another.
ARG ZISK_RELEASE=1.3.1-alpha
# The Rust toolchain tag of that release, for the image's labels. The pin file is the source: the build stops unless they agree.
ARG ZISK_TOOLCHAIN_TAG=zisk-4.0.0
ARG ROME_ZK_EVM_TAG=v0.3.0
ARG ROME_ZK_GUEST_TAG=v0.3.0

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

# The pin files of every release, and the one this image is built for, which guest-build.sh reads.
COPY zisk/ /usr/local/share/guest-build/zisk/

# ZisK itself, the CPU build, with no proving keys (those are about 26 GB and only the programVK step needs them).
#
# Every download is pinned by hash in the release's pin file. The toolchain tarball is fetched here, checked, and handed to
# ziskup through ZISK_TOOLCHAIN_SOURCE_DIR, so ziskup installs exactly that archive and never looks up the newest toolchain
# tag itself. ziskup downloads the cargo_zisk tarball on its own and has no hash check, so the same tarball is fetched and
# checked here as well, and every binary ziskup installed must be byte for byte the one in it.
RUN set -eu; \
    pins="/usr/local/share/guest-build/zisk/${ZISK_RELEASE}.env"; \
    pin() { sed -n "s/^$1=//p" "$pins" | head -n 1; }; \
    test -f "$pins" || { echo "ZiskReleaseUnknown: ${ZISK_RELEASE} has no pin file in zisk/" >&2; exit 1; }; \
    dup="$(grep -E '^[A-Z0-9_]+=' "$pins" | cut -d= -f1 | sort | uniq -d | head -n 1)"; \
    test -z "$dup" || { echo "PinFileInvalid: $pins names $dup more than once" >&2; exit 1; }; \
    ZISK_VERSION="$(pin ZISK_VERSION)"; test "${ZISK_VERSION}" = "${ZISK_RELEASE}"; \
    test "$(pin ZISK_TOOLCHAIN_TAG)" = "${ZISK_TOOLCHAIN_TAG}" || { echo "ToolchainTagMismatch: ZISK_TOOLCHAIN_TAG is ${ZISK_TOOLCHAIN_TAG}, but ZisK ${ZISK_RELEASE} uses $(pin ZISK_TOOLCHAIN_TAG); pass --build-arg ZISK_TOOLCHAIN_TAG=$(pin ZISK_TOOLCHAIN_TAG)" >&2; exit 1; }; \
    mkdir -p /tmp/zisk-pin/toolchain /tmp/zisk-pin/bundle; \
    curl -fsSL -o /tmp/zisk-pin/toolchain/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz "$(pin ZISK_TOOLCHAIN_URL)"; \
    echo "$(pin ZISK_TOOLCHAIN_SHA256)  /tmp/zisk-pin/toolchain/rust-toolchain-x86_64-unknown-linux-gnu.tar.gz" | sha256sum -c -; \
    curl -fsSL -o /tmp/zisk-pin/cargo_zisk.tar.gz "$(pin CARGO_ZISK_URL)"; \
    echo "$(pin CARGO_ZISK_SHA256)  /tmp/zisk-pin/cargo_zisk.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/zisk-pin/cargo_zisk.tar.gz -C /tmp/zisk-pin/bundle ./bin; \
    curl -fsSL -o /usr/local/bin/ziskup "$(pin ZISKUP_URL)"; \
    echo "$(pin ZISKUP_SHA256)  /usr/local/bin/ziskup" | sha256sum -c -; \
    chmod +x /usr/local/bin/ziskup; \
    ZISK_TOOLCHAIN_SOURCE_DIR=/tmp/zisk-pin/toolchain ziskup -v "${ZISK_VERSION}" --cpu --nokey -y --prefix /root/.zisk; \
    cargo-zisk --version | grep -F " ${ZISK_VERSION} " | grep -qF "($(pin ZISK_COMMIT | cut -c1-7)"; \
    rustup toolchain list | grep -q '^zisk'; \
    echo "${ZISK_VERSION}" > /usr/local/share/guest-build/release; \
    for f in /tmp/zisk-pin/bundle/bin/*; do \
      n=$(basename "$f"); \
      case "$n" in ziskup|*-gpu) continue ;; esac; \
      i="${n%-cpu}"; \
      cmp "$f" "/root/.zisk/bin/$i"; \
    done; \
    rm -rf /root/.zisk/cache/* /tmp/*

# The two public source trees. Submodules come with the guest. The two tags that were used are kept in /work for
# guest-build. The guest's Cargo.lock is checked against the image's release right after the clone.
COPY check-guest-lock.sh /usr/local/bin/check-guest-lock
RUN chmod +x /usr/local/bin/check-guest-lock
WORKDIR /work
RUN set -eu; \
    test -n "${ROME_ZK_EVM_TAG}" && test -n "${ROME_ZK_GUEST_TAG}"; \
    echo "${ROME_ZK_EVM_TAG}" > /work/rome-zk-evm.tag; \
    echo "${ROME_ZK_GUEST_TAG}" > /work/rome-zk-guest.tag; \
    git clone -q --branch "${ROME_ZK_EVM_TAG}" --depth 1 https://github.com/rome-protocol/solana-zk-evm.git rome-zk-evm; \
    git clone -q --branch "${ROME_ZK_GUEST_TAG}" --depth 1 https://github.com/rome-protocol/rome-zk-guest.git rome-zk-evm/.fork; \
    git -C rome-zk-evm/.fork submodule update --init --recursive -q; \
    git -C rome-zk-evm rev-parse HEAD > /work/rome-zk-evm.rev; \
    git -C rome-zk-evm/.fork rev-parse HEAD > /work/rome-zk-guest.rev; \
    check-guest-lock "$(cat /usr/local/share/guest-build/release)" /work/rome-zk-evm/.fork/bin/guests/stateless-validator-rome/Cargo.lock

COPY guest-build.sh /usr/local/bin/guest-build
RUN chmod +x /usr/local/bin/guest-build
LABEL org.opencontainers.image.title="rome-zk-guest-build" \
      org.opencontainers.image.description="Builds the Rome batch guest for one genesis with ZisK ${ZISK_RELEASE}" \
      zisk.version="${ZISK_RELEASE}" zisk.toolchain-tag="${ZISK_TOOLCHAIN_TAG}" \
      rome-zk-evm.tag="${ROME_ZK_EVM_TAG}" rome-zk-guest.tag="${ROME_ZK_GUEST_TAG}"

# /in/genesis.json is the genesis (read-only), /out receives the ELF and vkey.json, /keys/provingKey is the optional
# ZisK proving key directory (read-only). See guest-build.sh.
ENTRYPOINT ["/usr/local/bin/guest-build"]
