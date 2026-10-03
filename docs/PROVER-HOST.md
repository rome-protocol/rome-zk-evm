# Setting up a prover host

A rollup node settles on Solana only when a prover posts a proof for each batch. The prover runs ZisK 1.2.0-alpha on
one NVIDIA GPU. This page sets up a fresh Ubuntu host for it: the ZisK GPU build, its proving keys, the system
limits and the Docker runtime. The script that does it is
[`deploy/rollup/prover/setup-prover-host.sh`](../deploy/rollup/prover/setup-prover-host.sh), and every step is
described below so you can run it by hand instead.

You do not need a prover host to run the node itself, only to settle roots. See
[the prover is required for settlement](../deploy/rollup/README.md#the-prover-is-required-for-settlement).

## What the host needs

| Need | Value |
| --- | --- |
| Operating system | Ubuntu 22.04 or later, x86_64 |
| GPU | One NVIDIA GPU with more than 30 GB of memory. The final proof step asks for about 30 GB, so a 24 GB card is not enough. |
| NVIDIA driver | 525.60.13 or later, which is ZisK's own minimum. The prover image is built on CUDA 12.9, so a recent driver is the safer choice. |
| Disk | About 26 GB is downloaded into the install directory and about 81 GB is installed once unpacked (provingKey 53.7 GB, provingKeySnark 27.0 GB). Plan for about 125 GB free at the install directory while the download is unpacked. |
| Memory | The STARK step needed 37.5 GB of RAM and the PLONK step 88.4 GB when proved on a CPU host. Use a host with at least 128 GB of RAM until a GPU host has been measured. |

The disk figures were measured on a CPU host. A GPU host generates some files of its own, and those are not
measured yet.

## Run the script

Install an NVIDIA driver first if the host has none (`sudo ubuntu-drivers install`, then reboot). Then, as root:

```
sudo ZISK_HOME=/opt/zisk bash deploy/rollup/prover/setup-prover-host.sh
```

`ZISK_HOME` is where ZisK and the keys go. It becomes `ZISK_HOME` in your rollup `.env`, and the compose file mounts it
read-only into the prover container. Put it on a disk with room for the keys.

The script can be run again. It skips what is done, and it never downloads a key set twice. It stops at the first
problem with a line that starts with a name, and exits non-zero:

| Name | Meaning |
| --- | --- |
| `NotRoot` | Run it with `sudo`. |
| `UnsupportedPlatform` | ZisK's GPU build runs on Linux x86_64 only. |
| `NoGpuDriver`, `GpuDriverTooOld` | Install or update the NVIDIA driver. |
| `GpuMemoryTooSmall` | The largest GPU has less than 30 GB of memory. |
| `NotEnoughDisk` | Free more space at `ZISK_HOME`, or set `MIN_FREE_GB` if you know better. |
| `ZiskupFailed` | The ZisK installer failed. Its own output is above the line. |
| `KeysShaMismatch` | The keys under `ZISK_HOME` are not the pinned set. The script does not touch them. |
| `NotGpuBuild` | `cargo-zisk` is the CPU-only build. Move `ZISK_HOME` away and run the script again. |
| `WrongZiskVersion` | `cargo-zisk` is not 1.2.0-alpha. |
| `MemlockNotUnlimited` | The locked-memory limit is not unlimited in this shell. |
| `NoNvidiaDockerRuntime` | Docker has no `nvidia` runtime. |

Run `sudo ZISK_HOME=/opt/zisk bash deploy/rollup/prover/setup-prover-host.sh --check` at any time to repeat the checks
and change nothing.

## What the script does

1. **Checks the GPU.** It reads the driver version and the memory of the largest GPU with `nvidia-smi`.
2. **Installs the system packages** ZisK's binaries load: OpenMPI, OpenMP, GMP and libsodium, and rustup (minimal, with no default toolchain), which ziskup needs to install ZisK's toolchain.
3. **Removes the locked-memory limit.** `cargo-zisk` locks a large amount of memory while it proves and does not run
   under the default limit. The script writes `memlock unlimited` to `/etc/security/limits.d/` for login sessions and
   `DefaultLimitMEMLOCK=infinity` to the systemd configuration for services. Log out and in again for it to apply to
   your shell. The prover container sets the same limit itself in the compose file.
4. **Installs Docker and the NVIDIA container toolkit,** then registers the `nvidia` runtime with Docker and restarts it.
5. **Installs ZisK and the proving key** with the release's own installer:

   ```
   ziskup -v 1.2.0-alpha --gpu --provingkey -y --prefix "$ZISK_HOME"
   ZISK_DIR="$ZISK_HOME" ziskup setup_snark
   ```

   Two details matter. `ziskup` ignores a `ZISK_HOME` in the environment and installs into the home directory of whoever
   runs it, so the destination must be given with `--prefix`. And `ziskup setup_snark` has no `--prefix`: it installs the
   PLONK key into the directory in `ZISK_DIR`. The installer wipes and recreates its target on every run, so the script
   runs it only when the keys are not already there.
6. **Prepares the proving cache** at `$ZISK_HOME/cache`, owned by the user the prover container runs as (uid 999).
7. **Checks the result.** `cargo-zisk` must report the GPU build, the proving keys must match `keys.sha256`, the memlock
   limit must be unlimited and Docker must have the `nvidia` runtime.

## Check that cargo-zisk is the GPU build

```
$ZISK_HOME/bin/cargo-zisk --version
cargo-zisk 1.2.0-alpha [gpu] (...)
```

The tag in square brackets tells the two builds apart: `[gpu]` or `[cpu]`. The GPU build only uses the GPU when you
pass `-g`, and the prover does that for you when its config says `gpu = true`. The CPU-only build has no `-g`.

The prover never proves on the CPU by accident. `gpu` is a required line in `prover.toml`, and the rendered config
says `gpu = true`. With it, the prover refuses to start by name, `GpuBuildRequired`, when the `cargo-zisk` it finds is
not the GPU build. A config without a `gpu` line is refused too. To prove on the CPU on purpose, which takes hours per
batch, set `gpu = false`.

## The proving keys

`keys.sha256`, next to the script, holds one hash for each key directory. The hash covers every file's name and
content, so an added, removed, renamed or changed file shows up. It is pinned to the 1.2.0-alpha key set that Rome's
proofs were made with. The prover container checks it before every start, and you can check it yourself:

```
ZISK_HOME=/opt/zisk bash deploy/rollup/prover/check-keys.sh
```

It prints `OK` for each directory and exits 0. Any other exit code names the reason: 10 not populated, 12 mismatch,
13 manifest missing.

If the keys on your host are a different set (a newer release, say), the check refuses with `KeysShaMismatch` and the
prover does not start. Do not re-pin to make it pass. Install the release the manifest was made for. Pin your own
manifest with `--write` only if you are deliberately running a different release.

## Use the host for a rollup

Set these in the `.env` of `deploy/rollup`: `PROVER=on`, `ZISK_HOME` (the directory above), `VKEY_JSON` and `ELF_DIR`.
Then run `./rollup init` and `./rollup up`. The first `up` builds the prover image from this repository; the image
holds the prover and its runtime libraries, not ZisK or the keys. The compose file mounts `ZISK_HOME` read-only at
`/opt/zisk`, gives the container an unlimited memlock limit and reserves the NVIDIA GPU for it.

To confirm the GPU is visible from inside the container, once the image exists:

```
docker run --rm --gpus all --entrypoint nvidia-smi rome-zk-prover:local
docker run --rm --entrypoint /opt/zisk/bin/cargo-zisk -v "$ZISK_HOME:/opt/zisk:ro" rome-zk-prover:local --version
```

The first prints your GPU. The second must show `[gpu]`.
