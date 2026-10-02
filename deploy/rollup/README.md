# Run a rollup node

This folder runs one rollup chain on one machine with Docker Compose. You bring a Solana RPC endpoint, two key
files and a machine with Docker. You do not need a cloud account, and nothing here calls a cloud service.
Settling on Solana also needs one GPU for the prover.

You need bash, Docker with the compose plugin, curl, jq, openssl, Python 3.11 or newer, and rustup with cargo (the
toolchain `rust-toolchain.toml` at the repository root pins; rustup installs it on first use). The first `./rollup init`
compiles the client that reads your chain id from Solana, so it takes a few minutes. Docker Engine 28 or newer is
required: older engines let other machines on your network reach container ports that are bound to 127.0.0.1.

## What a full run needs

A full run settles on Solana. It needs the rollup's Solana programs on the cluster you point at, a prover with
one GPU, a verification key Rome has registered for your chain, and a guest built for your chain id. See
[The prover is required for settlement](#the-prover-is-required-for-settlement).

Rome's settlement programs are live on public Solana devnet. Their addresses are in
[`programs.devnet.json`](programs.devnet.json), the default `PROGRAMS_JSON` in `.env.example`, so `init` and
`check` now have the program-address file they need. The settlement program's global config enables
permissionless registration, with a 5 SOL registration deposit, a fee of 0.001 SOL per batch and a reclaim
window of 216000 slots.

Set `SOLANA_RPC_URL` in `.env` to any Solana devnet RPC endpoint you use. A provider endpoint is more reliable
than the public endpoint under load.

This folder contains the node and prover configuration and the `rollup` commands; it does not yet include the
command to build your chain's guest. The tests here run without the Solana programs.

## The four commands

Copy `.env.example` to `.env` and `chain.toml.example` to `chain.toml`, then edit both. `.env` holds this machine's
settings: your Solana RPC URL, the paths to your key files, who may reach the RPC. `chain.toml` describes the chain
itself: its genesis and the sequencer's settings. It has no chain id, because you do not choose one: the settlement
program derives it from your payer key and the number of chains that key has registered, and `init` reads it from Solana
(the payer keypair must exist at `init`).

```
./rollup init       # read your chain id from Solana, then write every config into rendered/
./rollup register   # register the chain on Solana (see below)
                    # then ask Rome to register your chain's verification key (see below)
./rollup up         # start the node
./rollup check      # one-screen health check
```

The order is `init`, `register`, ask Rome for the key, `up`. `register` needs only your Solana RPC: if the sequencer is
not running, it starts it for you (`./rollup up sequencer`), because registration records the genesis block it
reads from the sequencer.

`init` records the payer's public key, its registration count (the nonce) and the derived chain id in
`rendered/chain-id.env`, and renders the genesis and every config with that id. `register` reads the nonce again and
stops with `NonceAdvanced`, before sending anything, if the id it would now get is not the one in your genesis. It
sends the recorded nonce and chain id with the registration, so the program refuses a pair that went stale. Before any of that, `register` stops with `AuthorityChanged` if the payer key is not the one `init` recorded. If the nonce has moved because your own earlier run registered this chain, `register` sees the chain's root account and carries on instead of stopping.

`init` refuses by name a `[profile]` key in `chain.toml` that it sets itself (`ProfileKeyReserved`: `chain_id`,
`rpc_addr`, `metrics_addr`, `log_dir`, `sequencer_key_path`, `datadir`, `genesis_path`), and stops with
`ChainIdLookupFailed` if it cannot read your nonce from Solana.

`init` is safe to run again, also after `register`: once `rendered/chain-id.env` exists, `init` keeps the id in it,
even though registering moved your nonce forward, as long as the payer key is still the recorded authority
(`AuthorityChanged` otherwise). That is how you turn the prover on or change `ROME_ZK_TAG` on a registered chain. It
generates the engine secret and the prover's database password once and then leaves
them alone. It will not overwrite a genesis that differs from the one you already have, because that would give you
a different chain. Keep `rendered/` out of version control: it holds secrets and, if your Solana RPC URL carries an
API key, that too.

`register` needs one flag and has no default. `./rollup register --dry-run` prints the commands without running
anything. `./rollup register --confirm` runs them and writes `rendered/pdas.env`. Your payer keypair signs the
registration, and registration locks the chain deposit from it. The amount is set in the settlement program's global
config (5 SOL on devnet), so check it there before you fund the payer: the deposit is on top of the fees the batcher
pays.

`register --confirm` can be run again if it stopped after the registration was sent (for example the batch cursor
step failed). It finds the chain's root account on Solana, says the chain is already registered, does not register a
second time, and finishes the cursor and `rendered/pdas.env`. If it cannot read that account it stops with
`RootLookupFailed` and sends nothing.

`register` sends no verification key: the program refuses one on a permissionless chain. After it succeeds, send Rome
your chain id (it is in `rendered/chain-id.env`) and ask for your chain's verification key to be registered. The
chain has no verification key on Solana until Rome has registered it.

`up` starts the services. `./rollup up sequencer` starts just that one. `check` tells you, by name, which service is
missing or unhealthy, then compares the chain id, the heads, the batcher's progress, the inbox and the roots.

## What runs

| Service | What it does | Reachable from |
| --- | --- | --- |
| sequencer | the chain's public RPC: 8545 for HTTP, 8546 for WebSocket | `RPC_BIND`, default this machine only |
| reth-verifier | a second reth node that `derive` drives from the Solana inbox | 127.0.0.1 only |
| derive | rebuilds the chain from Solana alone and feeds reth-verifier | nothing (no RPC) |
| batcher | reads the sequencer's log and posts it to Solana | nothing (no RPC) |
| prover | required for settlement on Solana, see below | nothing (no RPC) |

The public RPC is the sequencer. To open it to the world, set `RPC_BIND=0.0.0.0` and put a TLS proxy in front
of it. Which proxy is up to you. This folder does not ship one. Set `RPC_BIND=0.0.0.0` only behind a firewall or that
proxy. `ufw` does not filter ports that Docker publishes, so with `RPC_BIND=0.0.0.0` a `ufw` rule alone does not close
the port: add rules to the `DOCKER-USER` chain or use your cloud provider's firewall.

## Which ports are bound where

| Port | Service | Bound to |
| --- | --- | --- |
| 8545 (HTTP) and 8546 (WebSocket) | sequencer, the chain's public RPC | `RPC_BIND`, which is 127.0.0.1 unless you change it. This is the only published port you may open to others. |
| 8547 | reth-verifier | 127.0.0.1 only |
| 8551 | reth-verifier engine API | not published |
| 9001 to 9004 | metrics of sequencer, batcher, derive and prover | 127.0.0.1 only |
| 5432 | the prover's postgres | 127.0.0.1 only, and only with the prover |

reth-verifier and derive are never reachable from outside the machine. reth-verifier runs reth's `testing` API, which
can include arbitrary transactions, so never publish its port or put it behind a proxy. The engine API (8551) is not
published at all. Any local user on the machine can reach the 127.0.0.1 ports, so run the node on a machine you do
not share.

## Metrics

Each service serves `GET /metrics` on its own port, on 127.0.0.1 only:

| Service | Port |
| --- | --- |
| sequencer | 9001 |
| batcher | 9002 |
| derive | 9003 |
| prover | 9004 |

No monitoring stack is shipped. Scrape these ports with whatever you already run.

## The prover is required for settlement

A prover is required for any root to settle on Solana. Every chain registered through this folder is
permissionless: its id is at or above `2^32`. The settlement program rejects unproved roots with
`UnprovedRootNotAllowed` (code 83). Only `PostRootProved` can settle a root for these chains.

Without a prover, the sequencer still serves the RPC. The batcher still posts the chain's batches to the Solana
inbox, and `derive` and `reth-verifier` rebuild the chain from Solana. No root becomes final on Solana, not even
the genesis root written at registration. Withdrawals need a final root, so no withdrawal can complete.

Proofs are checked against your chain's verification key. Ask Rome to register it after registering your chain,
as described above. `VKEY_JSON` must describe that registered key, and `ELF_DIR` must contain the matching guest
program built for your chain's id. The batch guest source is published at
[`rome-protocol/rome-zk-guest`](https://github.com/rome-protocol/rome-zk-guest), tag `v0.1.0`. Clone it as
`.fork/` inside a checkout of this repository, because it uses crates from this repository, then run
`git submodule update --init --recursive` inside `.fork/` before building. The command that builds the guest
for your chain id is not in this folder yet.

The prover needs one GPU and about 77 GB of proving keys downloaded once to the host. To turn it on, set `PROVER=on`
in `.env` together with `VKEY_JSON`, `ELF_DIR` and `ZISK_HOME`, then run `./rollup init` and `./rollup up`. A local
postgres container starts with it to hold the prover's history. Leave `PROVER` unset and none of this is rendered
or started.

The prover image is built from this tree: `prover/Dockerfile` is in this folder, and the first `./rollup up`
with `PROVER=on` builds it (from the repository root, which the compose file passes as the build context). You
can build it yourself with `docker build -f deploy/rollup/prover/Dockerfile -t rome-zk-prover:local .`. The
image does not contain the proving keys or the ZisK toolchain: you download those to `ZISK_HOME` and the
compose file mounts them.

Before every start the prover checks those keys against a manifest of hashes (`prover/keys.sha256`, checked by
`prover/check-keys.sh`, both mounted into the container). The manifest in this folder ships unpinned until the
release pins it, and an unpinned manifest stops the prover by name (`KeysManifestUnpinned`). To pin the keys you
downloaded, run this once on the host and keep the result:

```
ZISK_HOME=<the directory you downloaded the keys to> MANIFEST=./prover/keys.sha256 bash ./prover/check-keys.sh --write
```

The hash sorts file names bytewise (the script sets `LC_ALL=C` itself), so a pin made on a host with any `LANG` still matches inside the container.

To keep the manifest somewhere else, set `PROVER_KEYS_MANIFEST` in `.env` to its path.

## The node image

The compose file runs the published node image `ghcr.io/rome-protocol/rome-zk-evm:v0.1.1`. Public tags are
pinned to the commit of the source export they were built from, so a tag names exactly one tree and never
moves. Set `ROME_ZK_TAG=v0.1.1` in `.env`. `./rollup up` stops with `ImageTagNotSet` until you do. To run an
image you built from this tree yourself, set `ROME_ZK_IMAGE` to its full reference instead.

## Exits

The genesis carries the exit portal, the contract where a withdrawal starts, at `0x4200000000000000000000000000000000000016`.
Funds can leave the chain only after the settlement program's exit configuration names that portal. That configuration is
set later, through the program's governance, so a new chain has no working exit path until it is.

## Key files

Both are paths in `.env`, never the keys themselves. The containers run as uid 999, so the files must be
readable by that user.

- `SEQUENCER_KEY_PATH`: the sequencer's signing key, 64 hex characters in a file.
- `PAYER_KEYPAIR_PATH`: a Solana keypair (JSON) funded with SOL. It pays batch fees and is the chain authority.

## Running the tests

```
for t in deploy/rollup/tests/*.sh; do bash "$t"; done
```

They use fixtures and stubs only: no network, no Docker, no GPU.
