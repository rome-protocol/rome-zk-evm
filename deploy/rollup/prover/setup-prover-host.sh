#!/usr/bin/env bash
# Sets up a fresh Ubuntu GPU host to run the rollup prover: the ZisK 1.2.0-alpha GPU build and its proving keys under
# ZISK_HOME, an unlimited locked-memory limit, and Docker with the NVIDIA container toolkit. Run it once as root:
#
#   sudo ZISK_HOME=/opt/zisk bash setup-prover-host.sh
#
# It is safe to run again: a step that is already done is skipped, and a key set that is already installed is never
# downloaded a second time (ziskup wipes and recreates its install directory on every run).
#
# Modes:
#   (none)    install everything that is missing, then run the final checks
#   --check   change nothing; run the final checks only (GPU driver, GPU memory, cargo-zisk is the GPU build, the
#             proving keys against keys.sha256, the memlock limit, Docker with the NVIDIA runtime)
#
# Every refusal is a named line on stderr starting with its name, and a non-zero exit:
#   NotRoot, UnsupportedPlatform, NoGpuDriver, GpuDriverTooOld, GpuMemoryTooSmall, NotEnoughDisk, ZiskupFailed,
#   KeysShaMismatch, NotGpuBuild, WrongZiskVersion, MemlockNotUnlimited, NoNvidiaDockerRuntime
#
# What it needs from you: an NVIDIA GPU with more than 30 GB of memory (a 24 GB card is not enough), an NVIDIA driver
# at version 525.60.13 or later (install it first, see docs/PROVER-HOST.md; INSTALL_NVIDIA_DRIVER=1 installs the
# distribution's recommended driver and then stops so you can reboot), and about 125 GB of free disk at ZISK_HOME
# (about 26 GB is downloaded into ZISK_HOME and about 81 GB is installed; the download is deleted afterwards).
#
# Settings (environment):
#   ZISK_HOME             where ZisK and the keys are installed (default /opt/zisk)
#   ZISK_VERSION          the ZisK release (default 1.2.0-alpha; the keys manifest is pinned to this release's keys)
#   MANIFEST              the key hashes to check against (default: keys.sha256 next to this script)
#   MIN_FREE_GB           free disk required at ZISK_HOME before installing (default 125)
#   MIN_GPU_MIB           GPU memory required, in MiB (default 30720, which is 30 GiB)
#   INSTALL_NVIDIA_DRIVER set to 1 to install the recommended NVIDIA driver when none is present
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZISK_HOME="${ZISK_HOME:-/opt/zisk}"
ZISK_VERSION="${ZISK_VERSION:-1.2.0-alpha}"
MANIFEST="${MANIFEST:-$SCRIPT_DIR/keys.sha256}"
MIN_FREE_GB="${MIN_FREE_GB:-125}"
MIN_GPU_MIB="${MIN_GPU_MIB:-30720}"
NVIDIA_MIN_VERSION="525.60.13"
ZISKUP_BIN="${ZISKUP_BIN:-/usr/local/bin/ziskup}"
LIMITS_CONF="${LIMITS_CONF:-/etc/security/limits.d/90-zisk-memlock.conf}"
SYSTEMD_CONF="${SYSTEMD_CONF:-/etc/systemd/system.conf.d/90-zisk-memlock.conf}"
# The prover container runs as this user; it must be able to write the proving cache under ZISK_HOME/cache.
CONTAINER_UID="${CONTAINER_UID:-999}"

MODE="install"
case "${1:-}" in
  "") ;;
  --check) MODE="check" ;;
  *) echo "usage: setup-prover-host.sh [--check]" >&2; exit 1 ;;
esac

log() { echo "[setup-prover-host] $*"; }
refuse() { echo "$1: $2" >&2; exit 1; }

version_at_least() { # $1 = installed, $2 = minimum
  [[ "$(printf '%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

# --- 0. platform -----------------------------------------------------------------------------------------------
if [[ "$MODE" == "install" && "${EUID:-$(id -u)}" -ne 0 ]]; then
  refuse NotRoot "run this as root (sudo bash $0); it installs packages and writes system limits"
fi
if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  refuse UnsupportedPlatform "ZisK's GPU build runs on Linux x86_64 only (found $(uname -s) $(uname -m))"
fi

# --- 1. NVIDIA driver and GPU memory ---------------------------------------------------------------------------
check_gpu() {
  command -v nvidia-smi >/dev/null 2>&1 || return 1
  local drv mem
  drv="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -n1 | tr -d ' ')"
  version_at_least "$drv" "$NVIDIA_MIN_VERSION" \
    || refuse GpuDriverTooOld "NVIDIA driver $drv is older than the minimum $NVIDIA_MIN_VERSION that ZisK's GPU build needs"
  mem="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits | sort -n | tail -n1 | tr -d ' ')"
  [[ "$mem" =~ ^[0-9]+$ && "$mem" -ge "$MIN_GPU_MIB" ]] \
    || refuse GpuMemoryTooSmall "the largest GPU has ${mem:-unknown} MiB; the final proof step needs more than 30 GB (a 24 GB card is not enough)"
  log "GPU ok: $(nvidia-smi --query-gpu=name --format=csv,noheader | head -n1), driver $drv, ${mem} MiB"
}

if ! command -v nvidia-smi >/dev/null 2>&1; then
  if [[ "$MODE" == "install" && "${INSTALL_NVIDIA_DRIVER:-0}" == "1" ]]; then
    log "no NVIDIA driver found; installing the recommended one"
    apt-get update -y
    apt-get install -y --no-install-recommends ubuntu-drivers-common
    ubuntu-drivers install
    echo "RebootRequired: the NVIDIA driver was installed; reboot, then run this script again" >&2
    exit 1
  fi
  refuse NoGpuDriver "nvidia-smi not found. Install an NVIDIA driver at version $NVIDIA_MIN_VERSION or later (on Ubuntu: sudo ubuntu-drivers install, then reboot) or run with INSTALL_NVIDIA_DRIVER=1"
fi
check_gpu

# --- 2. packages, memlock, Docker and the NVIDIA container toolkit -----------------------------------------------
check_memlock() {
  [[ "$(ulimit -l)" == "unlimited" ]] \
    || refuse MemlockNotUnlimited "ulimit -l is $(ulimit -l), not unlimited. The limit is configured in $LIMITS_CONF and $SYSTEMD_CONF; log out and back in (or reboot) for it to apply to your shell"
}

check_docker_runtime() {
  command -v docker >/dev/null 2>&1 || refuse NoNvidiaDockerRuntime "docker is not installed"
  docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q nvidia \
    || refuse NoNvidiaDockerRuntime "Docker has no nvidia runtime; install nvidia-container-toolkit and run: nvidia-ctk runtime configure --runtime=docker && systemctl restart docker"
}

install_system() {
  log "installing the packages ZisK's binaries load (OpenMPI, OpenMP, GMP, sodium) and the tools ziskup needs"
  apt-get update -y
  apt-get install -y --no-install-recommends \
    ca-certificates curl gnupg jq xz-utils tar \
    libomp-dev libgmp-dev libsodium-dev libopenmpi-dev openmpi-bin openmpi-common

  # ziskup runs `cargo-zisk toolchain install`, which needs rustup. Install the minimal rustup (no default toolchain).
  if ! command -v rustup >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
    log "installing rustup (minimal, no default toolchain)"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
  fi

  # cargo-zisk shares memory between its processes and needs an unlimited locked-memory limit. Two places, because a
  # login shell reads limits.d and a systemd service (the Docker daemon, and so the container) reads system.conf.
  mkdir -p "$(dirname "$LIMITS_CONF")" "$(dirname "$SYSTEMD_CONF")"
  printf '* soft memlock unlimited\n* hard memlock unlimited\nroot soft memlock unlimited\nroot hard memlock unlimited\n' > "$LIMITS_CONF"
  printf '[Manager]\nDefaultLimitMEMLOCK=infinity\n' > "$SYSTEMD_CONF"
  log "memlock set to unlimited in $LIMITS_CONF and $SYSTEMD_CONF (takes effect at your next login; the prover container sets its own limit in the compose file)"

  if ! command -v docker >/dev/null 2>&1; then
    log "installing Docker"
    curl -fsSL https://get.docker.com | sh
  fi
  if ! docker compose version >/dev/null 2>&1; then
    apt-get install -y --no-install-recommends docker-compose-plugin
  fi
  if ! dpkg -s nvidia-container-toolkit >/dev/null 2>&1; then
    log "installing nvidia-container-toolkit"
    curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
      | gpg --dearmor --yes -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
    curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
      | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
      > /etc/apt/sources.list.d/nvidia-container-toolkit.list
    apt-get update -y
    apt-get install -y --no-install-recommends nvidia-container-toolkit
  fi
  nvidia-ctk runtime configure --runtime=docker
  systemctl restart docker
}

# --- 3. ZisK and the proving keys --------------------------------------------------------------------------------
keys_state() { # prints the check-keys.sh exit code
  local rc=0
  ZISK_HOME="$ZISK_HOME" MANIFEST="$MANIFEST" bash "$SCRIPT_DIR/check-keys.sh" >/dev/null 2>&1 || rc=$?
  echo "$rc"
}

free_gb_at() { # the nearest existing parent of $1
  local d="$1"
  while [[ ! -d "$d" ]]; do d="$(dirname "$d")"; done
  df -P -BG "$d" | awk 'NR==2 {gsub("G","",$4); print $4}'
}

install_zisk() {
  local state
  state="$(keys_state)"
  case "$state" in
    0) log "the keys under $ZISK_HOME already match $MANIFEST; skipping the download"; return 0 ;;
    12) refuse KeysShaMismatch "the keys under $ZISK_HOME do not match $MANIFEST; not touching them. Move the directory away and run this script again to download a fresh set" ;;
    10) ;;
    *) refuse KeysShaMismatch "check-keys.sh exited $state against $MANIFEST; run it by hand to see why" ;;
  esac

  local free
  free="$(free_gb_at "$ZISK_HOME")"
  [[ "$free" =~ ^[0-9]+$ && "$free" -ge "$MIN_FREE_GB" ]] \
    || refuse NotEnoughDisk "$ZISK_HOME has ${free:-unknown} GB free; the keys need about 26 GB to download and about 81 GB installed (set MIN_FREE_GB to override the $MIN_FREE_GB GB check)"

  if [[ ! -x "$ZISKUP_BIN" ]]; then
    log "fetching ziskup v$ZISK_VERSION"
    curl -fsSL -o "$ZISKUP_BIN" "https://raw.githubusercontent.com/0xPolygonHermez/zisk/v${ZISK_VERSION}/ziskup/ziskup"
    chmod +x "$ZISKUP_BIN"
  fi
  mkdir -p "$ZISK_HOME"
  # --prefix is what selects the install directory. ziskup exports a ZISK_HOME of its own but never reads one, so
  # setting ZISK_HOME alone would install into the home directory of whoever runs it.
  # ziskup downloads the key archives into the directory it is run from, so run it from ZISK_HOME: the download lands on
  # the disk the free-space check above measured. rustup puts cargo on PATH under ~/.cargo/bin, which ziskup needs.
  export PATH="$HOME/.cargo/bin:$PATH"
  log "installing the GPU build and the proving key into $ZISK_HOME (about 26 GB to download)"
  (cd "$ZISK_HOME" && "$ZISKUP_BIN" -v "$ZISK_VERSION" --gpu --provingkey -y --prefix "$ZISK_HOME") \
    || refuse ZiskupFailed "ziskup -v $ZISK_VERSION --gpu --provingkey failed"
  # The PLONK wrap needs a second key set. `ziskup setup_snark` has no --prefix; it installs into the directory in the
  # ZISK_DIR environment variable, and into the home directory when that is unset.
  log "installing the PLONK (snark) key"
  (cd "$ZISK_HOME" && ZISK_DIR="$ZISK_HOME" "$ZISKUP_BIN" setup_snark) \
    || refuse ZiskupFailed "ziskup setup_snark failed"
}

prepare_cache() {
  # cargo-zisk writes its witness and proof cache here while proving; a volume mounted over it by the compose file
  # starts out owned by the container user only if this directory is.
  mkdir -p "$ZISK_HOME/cache"
  chown "$CONTAINER_UID:$CONTAINER_UID" "$ZISK_HOME/cache"
}

# --- 4. the final checks -----------------------------------------------------------------------------------------
final_checks() {
  local bin="$ZISK_HOME/bin/cargo-zisk" line state
  [[ -x "$bin" ]] || refuse NotGpuBuild "$bin does not exist; ZisK is not installed under $ZISK_HOME"
  line="$("$bin" --version 2>&1 | head -n1)"
  case "$line" in
    *"[gpu]"*) ;;
    *) refuse NotGpuBuild "$bin is not the GPU build (its --version line says: $line). Move $ZISK_HOME away and run this script again" ;;
  esac
  [[ "$line" == *" $ZISK_VERSION "* ]] \
    || refuse WrongZiskVersion "$bin reports '$line'; this setup is for ZisK $ZISK_VERSION"
  log "cargo-zisk is the GPU build: $line"

  state="$(keys_state)"
  case "$state" in
    0) log "the proving keys match $MANIFEST" ;;
    *) refuse KeysShaMismatch "check-keys.sh exited $state against $MANIFEST (10 not populated, 11 unpinned, 12 mismatch); run it by hand to see which key directory differs" ;;
  esac
  if [[ "${SKIP_MEMLOCK_CHECK:-0}" != "1" ]]; then check_memlock; fi
  check_docker_runtime
  log "all checks passed"
}

if [[ "$MODE" == "install" ]]; then
  install_system
  install_zisk
  prepare_cache
  # The limit is only read by new login sessions; the checks below would fail in this one. Say so rather than fail.
  if [[ "$(ulimit -l)" != "unlimited" ]]; then
    log "memlock is configured but not yet active in this shell; log out and back in, then run: sudo bash $0 --check"
    SKIP_MEMLOCK_CHECK=1
  fi
fi
final_checks
