# Run a rollup node

This folder runs one rollup chain on one machine with Docker Compose. You bring a Solana RPC endpoint, two key
files and a machine with Docker. You do not need a cloud account, and nothing here calls a cloud service.
Settling on Solana also needs an NVIDIA GPU for the prover.

You need an x86-64 Linux machine with bash 4 or newer, Git, Docker Engine 28 or newer with the Compose plugin,
curl, jq, openssl, the Solana CLI, Python 3.11 or newer, a C compiler, and rustup with cargo. The repository pins the Rust
toolchain, and rustup installs it on the first build. On Debian or Ubuntu, run `sudo apt install build-essential`
for the compiler. The first `./rollup init` compiles the client that reads your chain id from Solana, so it
takes a few minutes. Older Docker engines can expose ports bound to 127.0.0.1 to other machines.

## What a full run needs

A full run settles on Solana. It needs the rollup's Solana programs on the cluster you point at, a prover with
one NVIDIA GPU with more than 30 GB of memory, a verification key Rome has registered for your chain,
and a guest built for your chain's genesis. See
[The prover is required for settlement](#the-prover-is-required-for-settlement).

Rome's settlement programs are live on public Solana devnet. Their addresses are in
[`programs.devnet.json`](programs.devnet.json), the default `PROGRAMS_JSON` in `.env.example`, so `init` and
`check` now have the program-address file they need. The settlement program's global config enables
permissionless registration, with a 5 SOL registration deposit and a 0.001 SOL fee when a proved root is
posted. The reclaim window is 6,480,000 slots, about 30 days at 400 ms per slot. After that, anyone
can reclaim a chain that has never posted a root. Its root, registry and chain
configuration accounts close. Their SOL, including the deposit, goes to the treasury, and its batcher
stops. The deposit is refundable to the chain authority after one final root or ten posted roots.

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
./rollup init                # read your chain id from Solana and render the configs
./rollup register --confirm  # register the chain on Solana
./rollup up                  # start the node
./rollup check               # health check
```

The node can run after `init` and `register --confirm`. To settle roots, you need a guest built for your
genesis and its verification key registered by Rome. If the sequencer is not running, registration starts
it with `./rollup up sequencer` so it can read the genesis block.

`init` records the payer's public key, its registration count (the nonce) and the derived chain id in
`rendered/chain-id.env`, and renders the genesis and every config with that id. `register` reads the nonce again and
stops with `NonceAdvanced`, before sending anything, if the id it would now get is not the one in your genesis. It
sends the recorded nonce and chain id with the registration, so the program refuses a pair that went stale. Before any of that, `register` stops with `AuthorityChanged` if the payer key is not the one `init` recorded. If the nonce has moved because your own earlier run registered this chain, `register` sees the chain's root account and carries on instead of stopping.

A new chain's genesis has no balances. Nobody is minted any coins, and the exit portal is the only predeployed
contract, at balance 0. This is on purpose. A genesis cannot change after you register, and a genesis that mints
coins could never take deposits safely: its holder could exit coins that other people's deposits paid for. L2 gas
comes from deposits once they ship, and until then a chain with no balances cannot send a transaction. If you need
gas sooner, you can declare one backed balance in `[genesis]`:

```toml
backed_address = "0x..."               # an address you control
backed_balance_lamports = 1_500_000_000  # 1.5 SOL, written in lamports
```

The amount is in lamports because it has to match what you lock in your chain's vault, which holds wrapped SOL.
One lamport is one gwei on your chain, and `init` renders wei as lamports times 1e9, so the conversion is always
exact. Write a plain whole number from 1 to 18446744073709551615, with no decimals, quotes, units, hex or exponents.
Set both keys or neither. You lock the same amount in the vault with the bridge program's `Fund` instruction, and Rome
checks that the vault holds it before it registers your verification key. `init` prints the exact lamport amount to
lock. First create the vault with the bridge program's `InitVault`, signed by your chain authority, which works only
after `register`; you also need that amount of wrapped SOL, on top of the SOL budget for registration and fees.
The bridge program is not deployed on devnet yet, so on devnet there is no vault to lock a backed balance in today.

`init` stops with `FundedAddressRemoved` if `chain.toml` still sets `genesis.funded_address`, which older versions
required and which minted 1e27 wei to that address. It also stops with `GenesisKeyUnknown` for any other key under
`[genesis]` that it does not know (including `backed_balance`, which has to carry its unit), `BackedAddressMissing`
or `BackedBalanceMissing` when only one of the two keys is set, `BackedAddressInvalid` for an address that is not `0x`
and 40 hex characters, `BackedAddressReserved` for zero, the precompile addresses (`0x00..00` to `0x00..ff`) and the exit
portal's address, and `BackedBalanceInvalid` for anything but a whole number of lamports in range. An existing
`rendered/genesis.json` is never changed, so adding a backed balance after your first `init` stops with `GenesisDrift`.

Set `genesis.fee_recipient` in `chain.toml` to an address you control. It receives the priority fees (tips) of the
chain's transactions; the base fee is burned, as on Ethereum. It becomes the genesis coinbase. The sequencer, derive
and the guest all read the fee recipient from there, and it cannot change once you register. `init` stops with `FeeRecipientMissing` if it is not set,
`FeeRecipientInvalid` if it is not `0x` and 40 hex characters, and `FeeRecipientReserved` for zero, the precompile
addresses (`0x00..00` to `0x00..ff`) and the exit portal's address.

A permissionless chain id falls anywhere from 2^32 to 2^53 - 1, and MetaMask cannot add a chain whose id is above
4503599627370476, which is about half of them. `init` checks the id it derived before you register: above that limit
it stops with `ChainIdNotWalletSafe` and sends nothing. Create a new payer key with `solana-keygen new`, point
`PAYER_KEYPAIR_PATH` at it, fund it and run `init` again; a different key gets a different id. If the chain is already
registered, its id is fixed, so `init` prints the same name as a warning and carries on.

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
config (5 SOL on devnet), so check it there before you fund the payer. Without a prover, Solana fees and
inbox account rent still use the payer's SOL. Watch its balance on a busy chain.

`register --confirm` can be run again if it stopped after the registration was sent (for example the batch cursor
step failed). It finds the chain's root account on Solana, says the chain is already registered, does not register a
second time, and finishes the cursor and `rendered/pdas.env`. If it cannot read that account it stops with
`RootLookupFailed` and sends nothing.

`register` sends no verification key: the program refuses one on a permissionless chain. Once you have a guest
built for your chain's genesis, [open an issue on rome-protocol/rome-zk-evm](https://github.com/rome-protocol/rome-zk-evm/issues) with the chain id from
`rendered/chain-id.env` and the guest ELF's sha256. Rome registers the matching verification key through the
registry authority. Until then, your chain has no verification key on Solana.

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

The numbers in the table are the defaults. Each host port can be moved in `.env` (`RPC_PORT`, `WS_PORT`, `VERIFIER_RPC_PORT`,
`SEQUENCER_METRICS_PORT`, `BATCHER_METRICS_PORT`, `DERIVE_METRICS_PORT`, `PROVER_METRICS_PORT`); run `./rollup init` again afterwards.

reth-verifier and derive are never reachable from outside the machine. reth-verifier also runs with peer discovery off,
no peer slots and its peer listener on 127.0.0.1, so it never dials the public Ethereum network; derive is its only
source of blocks. `./rollup check` fails if it ever has a peer. reth-verifier runs reth's `testing` API, which
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
[Monitoring one rollup](../../docs/MONITORING.md) explains what to watch and when to page someone.

## The prover is required for settlement

A prover is required for any root to settle on Solana. Every chain registered through this folder is
permissionless: its id is at or above `2^32`. The settlement program rejects unproved roots with
`UnprovedRootNotAllowed` (code 83). Only `PostRootProved` can settle a root for these chains.

Without a prover, the sequencer still serves the RPC. The batcher still posts the chain's batches to the Solana
inbox, and `derive` and `reth-verifier` rebuild the chain from Solana. No root becomes final on Solana, not even
the genesis root written at registration. Withdrawals need a final root, so no withdrawal can complete.

Proofs are checked against your chain's verification key. Ask Rome to register it after registering your chain,
as described above. `VKEY_JSON` must describe that registered key, and `ELF_DIR` must contain the matching guest
program built for your chain's genesis. The batch guest source is published at
[`rome-protocol/rome-zk-guest`](https://github.com/rome-protocol/rome-zk-guest), tag `v0.1.1`. From the root of
this repository, run `git clone --branch v0.1.1 https://github.com/rome-protocol/rome-zk-guest.git .fork`,
then `cd .fork && git submodule update --init --recursive`. The guest uses crates from this repository.
Building it for your `rendered/genesis.json` is not automated yet. Without that guest, your chain cannot
post a root.

The prover needs one NVIDIA GPU with more than 30 GB of memory and the ZisK proving keys on the host: about 26 GB to
download, about 81 GB once installed (measured on a CPU host; a GPU host generates its own files, not measured yet).
The final proof step needs about 30 GB of GPU memory; a 24 GB card is not enough. To turn it on, set `PROVER=on`
in `.env` together with `VKEY_JSON`, `ELF_DIR` and `ZISK_HOME`, then run `./rollup init` and `./rollup up`. A local
postgres container starts with it to hold the prover's history. Leave `PROVER` unset and none of this is rendered
or started.

The prover image is built from this tree: `prover/Dockerfile` is in this folder, and the first `./rollup up`
with `PROVER=on` builds it (from the repository root, which the compose file passes as the build context). You
can build it yourself with `docker build -f deploy/rollup/prover/Dockerfile -t rome-zk-prover:local .`. The
image does not contain the proving keys or the ZisK toolchain: you download those to `ZISK_HOME` and the
compose file mounts them.

Before every start the prover checks those keys against a manifest of hashes (`prover/keys.sha256`, checked by
`prover/check-keys.sh`, both mounted into the container). The manifest in this folder is pinned to the keys of
ZisK 1.2.0-alpha, and a mismatch stops the prover by name (`KeysShaMismatch`). The `*.consttree` files under
`provingKey` are not part of the hash: ziskup generates them on the host at install. Only to pin a key set of your own,
on purpose, run this once on the host and keep the result:

```
ZISK_HOME=<the directory you downloaded the keys to> MANIFEST=./prover/keys.sha256 bash ./prover/check-keys.sh --write
```

The hash sorts file names bytewise (the script sets `LC_ALL=C` itself), so a pin made on a host with any `LANG` still matches inside the container.

To keep the manifest somewhere else, set `PROVER_KEYS_MANIFEST` in `.env` to its path.

## The node image

The compose file runs the published node image `ghcr.io/rome-protocol/rome-zk-evm:v0.1.3`. Public tags are
pinned to the commit of the source export they were built from, so a tag names exactly one tree and never
moves. Set `ROME_ZK_TAG=v0.1.3` in `.env`. `./rollup up` stops with `ImageTagNotSet` until you do. To run an
image you built from this tree yourself, set `ROME_ZK_IMAGE` to its full reference instead.

## Exits

The genesis carries the exit portal, the contract where a withdrawal starts, at `0x4200000000000000000000000000000000000016`.
New chains start with no exit portal configured and an exit cap of zero, so exits are off. The chain authority
can use the settlement client's `governance` example with `propose-exit-config` to set the portal,
bridge program and cap.
After at least one 172,800-slot challenge window, anyone can send `activate-exit-config`. A final root
and a bridge program are also needed to release an exit. The devnet program set does not include a bridge yet.

## Key files

Both are paths in `.env`, never the keys themselves. The containers run as uid 999, so the files must be
readable by that user. From this directory, create them with the Solana CLI and openssl:

```sh
mkdir -p keys
solana-keygen new -o keys/payer.json
openssl rand -hex 32 > keys/sequencer.key
sudo chgrp 999 keys/payer.json keys/sequencer.key && chmod 640 keys/payer.json keys/sequencer.key
```

Fund the payer with about 6 devnet SOL, for example from [Solana's faucet](https://faucet.solana.com).
The host also reads the payer key during `init` and `register`. `.gitignore` in this directory excludes `keys/`.

- `SEQUENCER_KEY_PATH`: the sequencer's signing key, 64 hex characters in a file.
- `PAYER_KEYPAIR_PATH`: a Solana keypair (JSON) funded with SOL. It pays Solana fees and rent and is the chain authority.

## Running the tests

```
for t in deploy/rollup/tests/*.sh; do bash "$t"; done
```

They use fixtures and stubs only: no network, no Docker, no GPU.
