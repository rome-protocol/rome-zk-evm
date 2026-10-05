#!/usr/bin/env bash
# Sets up a fresh Ubuntu GPU host to run the rollup prover: the ZisK 1.3.1-alpha GPU build and its proving keys under
# ZISK_HOME, an unlimited locked-memory limit, and Docker with the NVIDIA container toolkit. Run it once as root:
#
#   sudo ZISK_HOME=/opt/zisk bash setup-prover-host.sh
#
# It is safe to run again: a step that is already done is skipped, and a key set that is already installed is never
# downloaded a second time.
#
# What is checked against the sha256 pins of the release, and when:
#   - the installer (ziskup), before it is run;
#   - both key archives, before either is unpacked. The script downloads them itself and unpacks the checked files;
#     ziskup runs with --nokey and downloads no key;
#   - the two GPU binaries cargo-zisk and cargo-zisk-dev, after ziskup has installed them and before this script runs
#     either of them.
# Nothing else is checked. In particular, ziskup downloads the release's binary archive (cargo_zisk_linux_amd64.tar.gz)
# with no hash check, unpacks it under ZISK_HOME, and runs `cargo-zisk toolchain install` from it as root, all before
# this script checks any binary. The rest of that archive (for example zisk-worker, zisk-coordinator, ziskemu, the libraries, the ZisK sources) is
# never checked.
#
# Modes:
#   (none)    install everything that is missing, then run the final checks
#   --check-ziskup
#             change nothing; check the installer at ZISKUP_BIN against the sha256 pinned for this release, and stop
#   --check-binaries
#             change nothing; check cargo-zisk and cargo-zisk-dev under ZISK_HOME against the pinned sha256 of the
#             release's GPU binaries, and stop
#   --check-archives
#             change nothing; check the two key archives in DOWNLOAD_DIR against their pinned sha256, and stop
#   --check   change nothing; run the final checks only (GPU driver, GPU memory, cargo-zisk is the pinned GPU build, the
#             proving keys against keys.sha256, the memlock limit, Docker with the NVIDIA runtime)
#
# Every refusal is a named line on stderr starting with its name, and a non-zero exit:
#   NotRoot, UnsupportedPlatform, NoGpuDriver, GpuDriverTooOld, GpuMemoryTooSmall, NotEnoughDisk, ZiskupFailed,
#   ZiskupHashMismatch, ZiskPinMissing, KeysShaMismatch, NotGpuBuild, WrongZiskVersion, MemlockNotUnlimited,
#   NoNvidiaDockerRuntime, KeyArchiveHashMismatch, KeyArchiveMissing, BinaryHashMismatch, KeysUrlMissing,
#   DownloadDirInsideZiskHome, ConstantFilesMissing
#
# What it needs from you: an NVIDIA GPU with at least 30,720 MiB (32.2 GB) of memory (a 24 GB card is not enough), an NVIDIA driver
# at version 525.60.13 or later (install it first, see docs/PROVER-HOST.md; INSTALL_NVIDIA_DRIVER=1 installs the
# distribution's recommended driver and then stops so you can reboot), and about 125 GB of free disk at ZISK_HOME.
# All sizes are decimal gigabytes (GB). About 27 GB is downloaded into DOWNLOAD_DIR and deleted afterwards; the
# unpacked keys take about 42 GB, about 62 GB once the constant trees are generated, and about 102 GB after the first
# proof.
#
# Settings (environment):
#   ZISK_HOME             where ZisK and the keys are installed (default /opt/zisk)
#   ZISK_VERSION          the ZisK release (default 1.3.1-alpha; the keys manifest is pinned to this release's keys)
#   PIN_FILE              the release's pin file (default guest-build/zisk/<ZISK_VERSION>.env, next to this folder); the
#                         ziskup sha256 and the commit of the release are read from it
#   ZISKUP_SHA256         the sha256 the installer must have (default: ZISKUP_SHA256 in the pin file)
#   ZISK_COMMIT           the commit of the release (default: ZISK_COMMIT in the pin file); cargo-zisk's version line
#                         must carry its first seven digits
#   HOST_PIN_FILE         the pins of the downloads this script checks itself (default host-pins/<ZISK_VERSION>.env next
#                         to this script): the sha256 of the two key archives and of the GPU cargo-zisk and
#                         cargo-zisk-dev inside the release's binary archive
#   ARCHIVE_SHA256, ARCHIVE_PLONK_SHA256, GPU_CARGO_ZISK_SHA256, GPU_CARGO_ZISK_DEV_SHA256
#                         override the matching line of HOST_PIN_FILE
#   KEYS_URL_BASE         where the key archives are downloaded from (default: the BUCKET_URL line of the checked ziskup, the
#                         place ziskup itself downloads them from; the script stops by name if neither gives a URL)
#   DOWNLOAD_DIR          where the key archives are downloaded to (default ZISK_HOME-downloads, beside ZISK_HOME); it must
#                         be on a disk with room for 27 GB and outside ZISK_HOME, which ziskup clears when it installs
#   MANIFEST              the key hashes to check against (default: keys.sha256 next to this script)
#   MIN_FREE_GB           free disk required at ZISK_HOME before installing, in GB (default 125)
#   MIN_GPU_MIB           GPU memory required, in MiB, the unit nvidia-smi reports (default 30720, which is 32.2 GB)
#   INSTALL_NVIDIA_DRIVER set to 1 to install the recommended NVIDIA driver when none is present
set -euo pipefail
export LC_ALL=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZISK_HOME="${ZISK_HOME:-/opt/zisk}"
ZISK_VERSION="${ZISK_VERSION:-1.3.1-alpha}"
MANIFEST="${MANIFEST:-$SCRIPT_DIR/keys.sha256}"
# Written once check-setup has finished for this release. It sits beside the key directories, not in them, so it is not
# part of their hash, and its name carries the release so another release's marker does not count.
SETUP_DONE="$ZISK_HOME/.check-setup-done-$ZISK_VERSION"
MIN_FREE_GB="${MIN_FREE_GB:-125}"
MIN_GPU_MIB="${MIN_GPU_MIB:-30720}"
NVIDIA_MIN_VERSION="525.60.13"
ZISKUP_BIN="${ZISKUP_BIN:-/usr/local/bin/ziskup}"
PIN_FILE="${PIN_FILE:-$SCRIPT_DIR/../guest-build/zisk/$ZISK_VERSION.env}"
HOST_PIN_FILE="${HOST_PIN_FILE:-$SCRIPT_DIR/host-pins/$ZISK_VERSION.env}"
KEYS_URL_BASE="${KEYS_URL_BASE:-}"
DOWNLOAD_DIR="${DOWNLOAD_DIR:-${ZISK_HOME%/}-downloads}"
LIMITS_CONF="${LIMITS_CONF:-/etc/security/limits.d/90-zisk-memlock.conf}"
SYSTEMD_CONF="${SYSTEMD_CONF:-/etc/systemd/system.conf.d/90-zisk-memlock.conf}"
# The prover container runs as this user; it must be able to write the proving cache under ZISK_HOME/cache.
CONTAINER_UID="${CONTAINER_UID:-999}"

MODE="install"
FETCHED_TMP=""

log() { echo "[setup-prover-host] $*"; }
refuse() { echo "$1: $2" >&2; exit 1; }

# The pins of this release: a KEY=VALUE line of a pin file, read as text (nothing in it is run by a shell). The release's
# own pin file (PIN_FILE, shared with the guest build) holds the ziskup sha256 and the commit; the host pin file
# (HOST_PIN_FILE) holds the hashes of what only this script downloads or installs.
pin_in() { # $1 = pin file, $2 = key -> its value, or nothing
  [[ -f "$1" ]] || return 0
  { grep -E "^$2=" "$1" || true; } | head -n1 | cut -d= -f2-
}
pin() { pin_in "$PIN_FILE" "$1"; }
host_pin() { pin_in "$HOST_PIN_FILE" "$1"; }
ZISKUP_SHA256="${ZISKUP_SHA256:-$(pin ZISKUP_SHA256)}"
ZISK_COMMIT="${ZISK_COMMIT:-$(pin ZISK_COMMIT)}"
ZISKUP_URL="${ZISKUP_URL:-$(pin ZISKUP_URL)}"
# The sha256 of the two key archives, and of the GPU cargo-zisk and cargo-zisk-dev inside the release's binary archive.
ARCHIVE_SHA256="${ARCHIVE_SHA256:-$(host_pin ARCHIVE_SHA256)}"
ARCHIVE_PLONK_SHA256="${ARCHIVE_PLONK_SHA256:-$(host_pin ARCHIVE_PLONK_SHA256)}"
GPU_CARGO_ZISK_SHA256="${GPU_CARGO_ZISK_SHA256:-$(host_pin GPU_CARGO_ZISK_SHA256)}"
GPU_CARGO_ZISK_DEV_SHA256="${GPU_CARGO_ZISK_DEV_SHA256:-$(host_pin GPU_CARGO_ZISK_DEV_SHA256)}"
ZISKUP_URL="${ZISKUP_URL:-https://raw.githubusercontent.com/0xPolygonHermez/zisk/v${ZISK_VERSION}/ziskup/ziskup}"

sha256_of() { # $1 = file -> its sha256
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# The installer is downloaded and then run as root, so it is checked against the sha256 pinned for the release before
# every run, whether this script fetched it or it was already on the host.
verify_ziskup() { # $1 = the file to check
  [[ "$ZISKUP_SHA256" =~ ^[0-9a-f]{64}$ ]] \
    || refuse ZiskPinMissing "no ziskup sha256 is pinned for ZisK $ZISK_VERSION (looked in $PIN_FILE; set ZISKUP_SHA256 to override); not running an installer that cannot be checked"
  local got
  got="$(sha256_of "$1")"
  [[ "$got" == "$ZISKUP_SHA256" ]] \
    || refuse ZiskupHashMismatch "$1 hashes to $got but ZisK $ZISK_VERSION pins $ZISKUP_SHA256; not running it. If it is an installer of another release, move it away and run this script again"
}

# Where the key archives are downloaded from: KEYS_URL_BASE if it is set, otherwise the BUCKET_URL line of the ziskup of
# this release, which is the place ziskup itself downloads the key archives from. The line is read as text from the
# installer after verify_ziskup has checked it (nothing in it is run or expanded); a value that is not a plain https:// or
# file:// URL is not used. With neither, the script stops by name rather than guess a host.
resolve_keys_url() { # $1 = the checked ziskup file
  [[ -z "$KEYS_URL_BASE" ]] || return 0
  local line plain='^(https|file)://[^[:space:]$"'"'"'`\\]+$'
  line="$({ grep -E '^BUCKET_URL=' "$1" || true; } | head -n1 | cut -d= -f2-)"
  line="${line%\"}"; line="${line#\"}"; line="${line%/}"
  if [[ "$line" =~ $plain ]]; then
    KEYS_URL_BASE="$line"
    return 0
  fi
  refuse KeysUrlMissing "no download location for the key archives: KEYS_URL_BASE is not set and $1 has no plain BUCKET_URL line (found: '${line:-nothing}'); set KEYS_URL_BASE to the folder that holds the key archives"
}

require_pin() { # $1 = the pin's name, $2 = its value
  [[ "$2" =~ ^[0-9a-f]{64}$ ]] \
    || refuse ZiskPinMissing "no $1 is pinned for ZisK $ZISK_VERSION (looked in $HOST_PIN_FILE; set $1 to override); not using a download that cannot be checked"
}

# A key archive is checked before it is unpacked: it holds the key files and the files ziskup generates the constant
# trees from, and the constant-tree step runs native code over them.
verify_archive() { # $1 = the archive file, $2 = its pin's name, $3 = the sha256 it must have
  require_pin "$2" "$3"
  local got
  got="$(sha256_of "$1")"
  [[ "$got" == "$3" ]] \
    || refuse KeyArchiveHashMismatch "$1 hashes to $got but ZisK $ZISK_VERSION pins $3; not unpacking it"
}

# The GPU binaries ziskup installs come from the release's binary archive, which ziskup downloads and unpacks itself with
# no hash check. Only these two files of it are checked, here, against the sha256 of the GPU binary inside that archive:
# after ziskup has installed them and before this script runs either of them.
verify_binaries() {
  local name var f want got
  for name in cargo-zisk cargo-zisk-dev; do
    if [[ "$name" == cargo-zisk ]]; then var=GPU_CARGO_ZISK_SHA256; else var=GPU_CARGO_ZISK_DEV_SHA256; fi
    f="$ZISK_HOME/bin/$name"
    [[ -f "$f" ]] || refuse NotGpuBuild "$f does not exist; ZisK is not installed under $ZISK_HOME"
    want="${!var}"
    require_pin "$var" "$want"
    got="$(sha256_of "$f")"
    [[ "$got" == "$want" ]] \
      || refuse BinaryHashMismatch "$f hashes to $got but the GPU build of ZisK $ZISK_VERSION pins $want; not running it. Move $ZISK_HOME away and run this script again"
  done
}

version_at_least() { # $1 = installed, $2 = minimum
  [[ "$(printf '%s\n' "$2" "$1" | sort -V | head -n1)" == "$2" ]]
}

# --- 1. NVIDIA driver and GPU memory ---------------------------------------------------------------------------
check_gpu() {
  command -v nvidia-smi >/dev/null 2>&1 || return 1
  local drv mem
  drv="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -n1 | tr -d ' ')"
  version_at_least "$drv" "$NVIDIA_MIN_VERSION" \
    || refuse GpuDriverTooOld "NVIDIA driver $drv is older than the minimum $NVIDIA_MIN_VERSION that ZisK's GPU build needs"
  mem="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits | sort -n | tail -n1 | tr -d ' ')"
  # nvidia-smi reports MiB; every size in this script's messages is in decimal GB.
  [[ "$mem" =~ ^[0-9]+$ && "$mem" -ge "$MIN_GPU_MIB" ]] \
    || refuse GpuMemoryTooSmall "the largest GPU has $( [[ "$mem" =~ ^[0-9]+$ ]] && echo "$((mem * 1048576 / 1000000000)) GB" || echo unknown ); the final proof step needs more than 30 GB (a 24 GB card is not enough)"
  log "GPU ok: $(nvidia-smi --query-gpu=name --format=csv,noheader | head -n1), driver $drv, $((mem * 1048576 / 1000000000)) GB"
}

ensure_gpu() {
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
}

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

free_gb_at() { # the free space at the nearest existing parent of $1, in decimal GB (df -k counts 1024-byte blocks)
  local d="$1"
  while [[ ! -d "$d" ]]; do d="$(dirname "$d")"; done
  df -Pk "$d" | awk 'NR==2 {printf "%d\n", $4 * 1024 / 1000000000}'
}

# Downloads one key archive into DOWNLOAD_DIR and leaves it there only once it has the pinned hash. The download goes to a
# .part file, which a run that was cut short resumes; the name without .part exists only for a checked archive.
fetch_archive() { # $1 = the archive's file name, $2 = its pin's name, $3 = the sha256 it must have
  local name="$1" file="$DOWNLOAD_DIR/$1" part="$DOWNLOAD_DIR/$1.part" rc=0 got
  require_pin "$2" "$3"
  if [[ -f "$file" && "$(sha256_of "$file")" == "$3" ]]; then
    log "$name is already downloaded and matches its pin"
    return 0
  fi
  rm -f "$file"
  log "downloading $name"
  curl -fL --retry 3 -C - -o "$part" "$KEYS_URL_BASE/$name" || rc=$?
  [[ -f "$part" ]] || refuse ZiskupFailed "could not download $KEYS_URL_BASE/$name"
  got="$(sha256_of "$part")"
  if [[ "$got" != "$3" ]]; then
    # A download that stopped early has the wrong hash too, but it is not a bad archive: keep it to resume.
    [[ $rc -eq 0 ]] || refuse ZiskupFailed "could not finish downloading $KEYS_URL_BASE/$name (curl exit $rc); run this script again to resume, or delete $part to start over"
    rm -f "$part"
    refuse KeyArchiveHashMismatch "$name hashes to $got but ZisK $ZISK_VERSION pins $3; not unpacking it"
  fi
  mv "$part" "$file"
}

# The proving key, then the PLONK key, unpacked from the archives checked above. This is what ziskup would download and
# unpack itself (it checks only an md5 that comes from the same bucket as the archive).
install_keys() {
  local pk="zisk-provingkey-$ZISK_VERSION.tar.gz" snark="zisk-provingkey-plonk-$ZISK_VERSION.tar.gz"
  rm -f "$SETUP_DONE"
  log "unpacking the proving key into $ZISK_HOME"
  rm -rf "$ZISK_HOME/provingKey" "$ZISK_HOME/verifyKey" "$ZISK_HOME/cache"
  tar --no-same-owner --overwrite -xf "$DOWNLOAD_DIR/$pk" -C "$ZISK_HOME" \
    || refuse ZiskupFailed "could not unpack $DOWNLOAD_DIR/$pk"
  rm -f "$DOWNLOAD_DIR/$pk"
  log "unpacking the PLONK (snark) key into $ZISK_HOME"
  rm -rf "$ZISK_HOME/provingKeySnark"
  tar --no-same-owner --overwrite -xf "$DOWNLOAD_DIR/$snark" -C "$ZISK_HOME" \
    || refuse ZiskupFailed "could not unpack $DOWNLOAD_DIR/$snark"
  rm -f "$DOWNLOAD_DIR/$snark"
  rmdir "$DOWNLOAD_DIR" 2>/dev/null || true
}

# The constant trees, and on a GPU host the *.const_gpu files, are generated from both keys by the binary checked above.
# The generated files are left out of the key hash, so a run that stopped here would look finished to the key check; the
# done marker is what says this step ran to the end. Aggregation stays on: in check-setup, -a means --no-aggregation, and
# without the compressor and recursive files every proof stops at its first aggregation step. The key paths are explicit
# because the default is the home directory of whoever runs this, and under sudo that is root's.
run_check_setup() {
  [[ ! -e "$SETUP_DONE" ]] || return 0
  log "generating the constant trees and GPU constant files for both keys (several minutes)"
  (cd "$ZISK_HOME" && ZISK_HOME="$ZISK_HOME" "$ZISK_HOME/bin/cargo-zisk-dev" check-setup --proving-key "$ZISK_HOME/provingKey" \
    --proving-key-plonk "$ZISK_HOME/provingKeySnark" --plonk --gpu >/dev/null) \
    || refuse ZiskupFailed "cargo-zisk-dev check-setup failed on the proving keys; run this script again to repeat it"
  : > "$SETUP_DONE"
}

# The last check: the step above has finished for this release.
require_setup_done() {
  [[ -e "$SETUP_DONE" ]] \
    || refuse ConstantFilesMissing "the constant files have not been generated for ZisK $ZISK_VERSION ($SETUP_DONE is missing); run this script again, or by hand: cd $ZISK_HOME && ZISK_HOME=$ZISK_HOME $ZISK_HOME/bin/cargo-zisk-dev check-setup --proving-key $ZISK_HOME/provingKey --proving-key-plonk $ZISK_HOME/provingKeySnark --plonk --gpu \&\& touch $SETUP_DONE"
}

install_zisk() {
  # ziskup clears ZISK_HOME when it installs, so archives kept inside it would be gone before they are unpacked.
  case "${DOWNLOAD_DIR%/}/" in
    "${ZISK_HOME%/}/"*) refuse DownloadDirInsideZiskHome "DOWNLOAD_DIR ($DOWNLOAD_DIR) is inside ZISK_HOME ($ZISK_HOME), which ziskup clears when it installs; set DOWNLOAD_DIR to a directory outside it" ;;
  esac
  local state
  state="$(keys_state)"
  case "$state" in
    0) log "the keys under $ZISK_HOME already match $MANIFEST; skipping the download"
       if [[ ! -e "$SETUP_DONE" ]]; then verify_binaries; run_check_setup; fi
       return 0 ;;
    12) refuse KeysShaMismatch "the keys under $ZISK_HOME do not match $MANIFEST; not touching them. Move the directory away and run this script again to download a fresh set" ;;
    10) ;;
    *) refuse KeysShaMismatch "check-keys.sh exited $state against $MANIFEST; run it by hand to see why" ;;
  esac

  local free
  free="$(free_gb_at "$ZISK_HOME")"
  [[ "$free" =~ ^[0-9]+$ && "$free" -ge "$MIN_FREE_GB" ]] \
    || refuse NotEnoughDisk "$ZISK_HOME has ${free:-unknown} GB free; the keys need about 27 GB to download and about 50 GB on disk once the constant files are generated, and the first proof needs room for its cache on top (set MIN_FREE_GB to override the $MIN_FREE_GB GB check)"

  if [[ ! -e "$ZISKUP_BIN" ]]; then
    # Fetch to a file of its own and check it there: an installer that fails the check never sits at $ZISKUP_BIN.
    FETCHED_TMP="$(mktemp)"
    log "fetching ziskup v$ZISK_VERSION"
    curl -fsSL -o "$FETCHED_TMP" "$ZISKUP_URL" || refuse ZiskupFailed "could not download $ZISKUP_URL"
    verify_ziskup "$FETCHED_TMP"
    install -m 0755 "$FETCHED_TMP" "$ZISKUP_BIN"
    rm -f "$FETCHED_TMP"
  fi
  verify_ziskup "$ZISKUP_BIN"
  resolve_keys_url "$ZISKUP_BIN"
  mkdir -p "$ZISK_HOME" "$DOWNLOAD_DIR"
  # The key archives are downloaded and checked first, so a bad one stops the install before ziskup runs and before either
  # archive is unpacked.
  fetch_archive "zisk-provingkey-$ZISK_VERSION.tar.gz" ARCHIVE_SHA256 "$ARCHIVE_SHA256"
  fetch_archive "zisk-provingkey-plonk-$ZISK_VERSION.tar.gz" ARCHIVE_PLONK_SHA256 "$ARCHIVE_PLONK_SHA256"
  # --prefix is what selects the install directory. ziskup exports a ZISK_HOME of its own but never reads one, so
  # setting ZISK_HOME alone would install into the home directory of whoever runs it. --nokey: ziskup installs the
  # binaries only; the keys are the checked archives above. rustup puts cargo on PATH under ~/.cargo/bin, which ziskup
  # needs (it runs `cargo-zisk toolchain install`).
  export PATH="$HOME/.cargo/bin:$PATH"
  log "installing the GPU build into $ZISK_HOME"
  (cd "$ZISK_HOME" && "$ZISKUP_BIN" -v "$ZISK_VERSION" --gpu --nokey -y --prefix "$ZISK_HOME") \
    || refuse ZiskupFailed "ziskup -v $ZISK_VERSION --gpu --nokey failed"
  # ziskup has already run an unchecked cargo-zisk (its toolchain step); from here on this script runs only the two
  # binaries it has checked.
  verify_binaries
  install_keys
  run_check_setup
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
  # The binaries are hashed before either is run: the version line below is what a binary says about itself.
  verify_binaries
  line="$("$bin" --version 2>&1 | head -n1)"
  case "$line" in
    *"[gpu]"*) ;;
    *) refuse NotGpuBuild "$bin is not the GPU build (its --version line says: $line). Move $ZISK_HOME away and run this script again" ;;
  esac
  [[ "$line" == *" $ZISK_VERSION "* ]] \
    || refuse WrongZiskVersion "$bin reports '$line'; this setup is for ZisK $ZISK_VERSION"
  # The version line ends "(<first seven digits of the commit> <build date>)". Two builds can share a release name.
  [[ "$ZISK_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
    || refuse ZiskPinMissing "no commit is pinned for ZisK $ZISK_VERSION (looked in $PIN_FILE; set ZISK_COMMIT to override)"
  [[ "$line" == *"(${ZISK_COMMIT:0:7} "* ]] \
    || refuse WrongZiskVersion "$bin reports '$line'; ZisK $ZISK_VERSION is built from commit ${ZISK_COMMIT:0:7}"
  log "cargo-zisk is the GPU build: $line"

  state="$(keys_state)"
  case "$state" in
    0) log "the proving keys match $MANIFEST" ;;
    *) refuse KeysShaMismatch "check-keys.sh exited $state against $MANIFEST (10 not populated, 11 unpinned, 12 mismatch); run it by hand to see which key directory differs" ;;
  esac
  require_setup_done
  if [[ "${SKIP_MEMLOCK_CHECK:-0}" != "1" ]]; then check_memlock; fi
  check_docker_runtime
  log "all checks passed"
}

main() {
  case "${1:-}" in
    "") ;;
    --check) MODE="check" ;;
    --check-ziskup) MODE="check-ziskup" ;;
    --check-binaries) MODE="check-binaries" ;;
    --check-archives) MODE="check-archives" ;;
    *) echo "usage: setup-prover-host.sh [--check | --check-ziskup | --check-binaries | --check-archives]" >&2; exit 1 ;;
  esac
  trap '[[ -z "$FETCHED_TMP" ]] || rm -f "$FETCHED_TMP"' EXIT

  case "$MODE" in
    check-ziskup)
      [[ -f "$ZISKUP_BIN" ]] || refuse ZiskupFailed "there is no installer at $ZISKUP_BIN"
      verify_ziskup "$ZISKUP_BIN"
      log "$ZISKUP_BIN is the ziskup of ZisK $ZISK_VERSION ($ZISKUP_SHA256)"
      exit 0 ;;
    check-binaries)
      verify_binaries
      log "cargo-zisk and cargo-zisk-dev under $ZISK_HOME are the GPU build of ZisK $ZISK_VERSION"
      exit 0 ;;
    check-archives)
      local a
      for a in "zisk-provingkey-$ZISK_VERSION.tar.gz:ARCHIVE_SHA256:$ARCHIVE_SHA256" "zisk-provingkey-plonk-$ZISK_VERSION.tar.gz:ARCHIVE_PLONK_SHA256:$ARCHIVE_PLONK_SHA256"; do
        [[ -f "$DOWNLOAD_DIR/${a%%:*}" ]] || refuse KeyArchiveMissing "there is no ${a%%:*} in $DOWNLOAD_DIR"
        verify_archive "$DOWNLOAD_DIR/${a%%:*}" "$(echo "$a" | cut -d: -f2)" "$(echo "$a" | cut -d: -f3)"
        log "${a%%:*} matches its pin"
      done
      exit 0 ;;
  esac

  # --- 0. platform -----------------------------------------------------------------------------------------------
  if [[ "$MODE" == "install" && "${EUID:-$(id -u)}" -ne 0 ]]; then
    refuse NotRoot "run this as root (sudo bash $0); it installs packages and writes system limits"
  fi
  if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
    refuse UnsupportedPlatform "ZisK's GPU build runs on Linux x86_64 only (found $(uname -s) $(uname -m))"
  fi
  ensure_gpu
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
}

# Run when executed; a test that sources this file calls the functions above on its own.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then main "$@"; fi
