# Run a rollup node

This folder runs one rollup chain on one machine with Docker Compose. You bring a Solana RPC endpoint, two key
files and a machine with Docker. You do not need a cloud account, and nothing here calls a cloud service.
Settling on Solana also needs an NVIDIA GPU for the prover.

You need an x86-64 Linux machine with bash 4 or newer, Git, Docker Engine 28 or newer with the Compose plugin,
curl, jq, openssl, the Solana CLI and Python 3.11 or newer. You do not need Rust or a compiler: everything the
`rollup` command does on Solana runs inside the node image, so the first command that talks to Solana pulls the image
(set `ROME_ZK_TAG` first, see [The node image](#the-node-image)). Older Docker engines can expose ports bound to
127.0.0.1 to other machines.

## What a full run needs

A full run settles on Solana. It needs the rollup's Solana programs on the cluster you point at, a prover with
one NVIDIA GPU with more than 30 GB of memory, a verification key Rome has registered for your chain,
and a guest built for your chain's genesis. See
[The prover is required for settlement](#the-prover-is-required-for-settlement).

Rome's settlement programs are live on public Solana devnet. Their addresses are in
[`programs.devnet.json`](programs.devnet.json), the default `PROGRAMS_JSON` in `.env.example`, and `init` and
`check` read the program addresses from it. The settlement program's global config enables
permissionless registration, with a 5 SOL registration deposit and a 0.001 SOL fee when a proved root is
posted. The reclaim window is 6,480,000 slots, about 30 days at 400 ms per slot. After that, anyone
can reclaim a chain that has never posted a root. Its root, registry and chain
configuration accounts close. Their SOL, including the deposit, goes to the treasury, and its batcher
stops. The deposit is refundable to the chain authority after one final root or ten posted roots. `./rollup refund-deposit --confirm` sends it back.

Set `SOLANA_RPC_URL` in `.env` to any Solana devnet RPC endpoint you use. A provider endpoint is more reliable
than the public endpoint under load.

This folder contains the node and prover configuration and the `rollup` commands. The tests here run without
the Solana programs.

## The commands

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

These four set a chain up. The rest of the commands are listed under [Operating the chain](#operating-the-chain).

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
On devnet the bridge program is the shared zk-bridge in `programs.devnet.json`. [Your chain's vault](#your-chains-vault)
gives the commands.

`init` stops with `FundedAddressRemoved` if `chain.toml` sets `genesis.funded_address`. It also stops with
`GenesisKeyUnknown` for any other key under `[genesis]` that it does not know (including `backed_balance`, which has to carry its unit), `BackedAddressMissing`
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
built for your chain's genesis, [open an issue on this repository](https://github.com/rome-protocol/solana-zk-evm/issues).
Attach `rendered/genesis.json` and `rendered/guest/vkey.json`, and include your chain id from
`rendered/chain-id.env` and the guest tag. `vkey.json` holds the ELF's sha256 (`elf_sha256`), the genesis
file's sha256 (`genesis_sha256`) and the `programVK`. If your genesis declares a balance, include the vault
that backs it. Rome answers on the issue. Until your key is registered, your chain has no verification key on
Solana.

`up` starts the services. `./rollup up sequencer` starts just that one. `check` tells you, by name, which service is
missing or unhealthy, then compares the chain id, the heads, the batcher's progress, the inbox and the roots. Its items
are listed under [What `check` looks at](#what-check-looks-at).

## Operating the chain

Every command that sends a Solana transaction is a dry run unless you add `--confirm`. A dry run reads the chain, prints
the transaction it would send and sends nothing. `--offline` is a dry run with no network at all, and `--confirm` with
`--dry-run` or `--offline` is refused with `ConfirmAndDryRun`. Each command runs `rome-zk-ops` from the node image, so
`ROME_ZK_TAG` (or `ROME_ZK_IMAGE`) must be set. Your payer key is mounted into that container read-only for the one
command and is never printed. Run `init` first: the commands read the chain from `rendered/`.

| Command | What it does |
| --- | --- |
| `./rollup status` | the running services, then what Solana says about the chain: slot, last pending and final batch, the reclaim deadline, the deposit and whether a verification key is active |
| `./rollup logs [-f] [--tail N] [service...]` | the services' logs, the last 200 lines each by default |
| `./rollup down` | stops and removes the containers; the data volumes stay, so the chain's data is safe |
| `./rollup refund-deposit [--confirm]` | gives the registration deposit back to the chain authority |
| `./rollup exit-config propose ... [--confirm]` | proposes a new exit portal, bridge program, cap or poster bond (see [Exits](#exits)) |
| `./rollup exit-config activate [--confirm]` | activates a proposal once its slot has passed |
| `./rollup exit-config show` | prints the exit configuration and any pending change |
| `./rollup vault init\|fund\|show` | creates, funds and reads your chain's vault (see [Your chain's vault](#your-chains-vault)) |
| `./rollup deposit-queue init [parameter flags] [--confirm]` | creates your chain's deposit queue with the standard parameters (a 12 hour deadline, 256 deposits a batch, 4 a block, a 0.001 SOL minimum, a 0.0001 SOL fee paid to your payer key); the chain must have posted a root first, and the per-block limit is checked against the blocks per batch `init` rendered |
| `./rollup deposit-queue propose [parameter flags] [--activation-slot N \| --activation-delay-slots N] [--confirm]` | proposes new parameters; a new proposal replaces a pending one, and the activation slot must be between one and two challenge windows away |
| `./rollup deposit-queue activate [--confirm]` | makes the pending parameters live once their slot has passed |
| `./rollup deposit-queue show` | prints the live and pending parameters, how many deposits the queue holds, how many the chain has taken, and how long the oldest waiting deposit has waited against the deadline |
| `./rollup deposit --amount N --recipient 0x... --keypair FILE [--wrap-sol] [--confirm]` | deposits `N` raw token units from the depositor's own key, to be credited to the 20-byte address on your chain |
| `./rollup close-deposit --index N [--confirm]` | gives a deposit record's rent back to its depositor once the batch that credited it is final |
| `./rollup release-exit --message-hash 0x... [--confirm]` | releases a proved exit from the vault to its recipient |
| `./rollup migrate --registry-keypair FILE --max-drift-secs N [--confirm]` | brings an older chain's accounts forward; only the registry authority can run it |

A refusal always starts with its name, for example `NothingProposed` or `KeyFileMissing`, and nothing is sent.

## What `check` looks at

`./rollup check` prints `PASS`, `FAIL`, `WARN` or `SKIP` for each item, names every failure, and exits non-zero
when any item failed. A `WARN` is shown but does not fail the run. An item that cannot be judged because a service
is not running is skipped with the reason.

| Item | Passes when | Fails by name |
| --- | --- | --- |
| service | each service is running | `ServiceMissing` |
| chain id, sequencer head, derive lag, verifier peers | the chain id matches, blocks (or the idle counter) advance, the verifier keeps pace and has no peers | `ChainIdMismatch`, `SequencerStalled`, `DeriveBehind`, `VerifierHasPeers` |
| batcher cursor, inbox batches, roots | the accounts on Solana read, and the sequencer is not two batches' worth of blocks ahead of the posted batches | `CursorUnreadable`, `NoBatchPosted`, `BatcherBehind`, `BlocksPerBatchUnreadable`, `RootUnreadable` |
| batches finalized | blocks were sealed and the batcher finalized a batch since the first sample from at least the window ago | `BatchesNotFinalizing` |
| settlement lag, prover lag (`PROVER=on`) | the prover is at most 2 batches behind | `SettlementBehind`, `ProverBehind` |
| verification key (`PROVER=on`) | a verification key for the proving layout is active in the registry | `VkeyNotActive` |
| payer balance | the payer holds at least the floor | `PayerBelowFloor` |
| reclaim deadline | the chain has posted a root, or the deadline is further away than the margin | `ReclaimDeadlineNear` inside the margin, `ReclaimDeadlinePassed` once it is over |
| exit config | the exit configuration reads, and is shown | `ExitConfigUnreadable` |
| deposit-capable key | before a deposit queue exists, the registry's active key equals the `programVK` in the `vkey.json` that `./rollup guest-build` wrote from a guest at v0.2.0 or later (a `WARN` when `vkey.json` records no guest tag) | `GuestNotDepositCapable`, `DepositKeyNotActive`, `DepositKeyNotRegistered` |
| deposit caps | the per-block limit times the blocks per batch fits the per-batch limit, for the live parameters and for a pending proposal | `DepositCapsExceedBatch`, `BlocksPerBatchUnreadable` |
| genesis balance | the genesis gives at most one account a balance, and the vault holds it until the chain has a final batch (afterwards the vault's balance is shown, since exits move it); before the deposit queue exists a missing or short vault is a `WARN` | `GenesisFundedAccountLimit`, `GenesisBalanceNotWholeLamports`, `GenesisUnreadable`, `VaultMissing`, `VaultBelowGenesis`, `VaultUnreadable` |
| exit config bridge | once the deposit queue exists, the exit configuration names the bridge program in `programs.devnet.json` (before it exists a missing exit configuration is skipped; one that names another bridge always fails) | `ExitConfigMissing`, `ExitConfigNamesNoBridge`, `ExitConfigBridgeMismatch` |
| deposits section | a deposit queue exists only when the sequencer's config has a `[deposits]` section | `DepositsSectionMissing` |
| deposit cursor | the batch cursor is in the deposit-aware format once the chain has finalized a batch after the upgrade | `CursorNotV2` |
| oldest deposit | the oldest waiting deposit has waited less than three quarters of the deadline, the shorter of the live and a pending one (a `WARN` past half) | `DepositNearDeadline` |
| deposit backlog | shows the deposits the queue holds that no batch has taken yet | `DepositQueueUnreadable` |

The batcher's own age gauge resets each time a batch closes by age and stands still while the batcher waits on Solana,
so it cannot see a stuck batcher. `check` compares two counters instead: blocks the sequencer sealed and batches the
batcher finalized. Each run records both in `rendered/check-samples`. From the first sample at least the window old, if
blocks were sealed and no batch finalized, the batcher is stuck, which is the same rule the
`RomeZkBatchesNotFinalizing` alert in [`docs/monitoring/alerts.example.yml`](../../docs/monitoring/alerts.example.yml) uses. So run
`./rollup check` on a timer (every few minutes): the first run only records a sample and says so, and the item
judges from the second run on. A restart of either process resets its counter, and a sample from before a reset is not used.

Settings, each in `.env` or the environment:

| Setting | Default | Meaning |
| --- | --- | --- |
| `PAYER_FLOOR_LAMPORTS` | 1000000000 (1 SOL) | the payer balance below which `check` fails |
| `RECLAIM_MARGIN_SLOTS` | 108000 (about 12 hours) | `check` fails when fewer slots than this are left before anyone can reclaim a chain that has not posted a root |
| `CHECK_FINALIZE_WINDOW_SECS` | the larger of 900 and four times `BATCH_CLOSE_AFTER_SECS` | how long blocks may go without a batch finalizing |

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
| 5432 | the prover's postgres | not published; only the prover reaches it, over the compose network |

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
[`rome-protocol/rome-zk-guest`](https://github.com/rome-protocol/rome-zk-guest), tag `v0.2.0`. From the root of
this repository, run `git clone --branch v0.2.0 https://github.com/rome-protocol/rome-zk-guest.git .fork`,
then `cd .fork && git submodule update --init --recursive`. The guest uses crates from this repository.
Build it with `./rollup guest-build`. The command builds the guest-build image (the toolchain of one ZisK release,
1.2.0-alpha by default, and the default stays that until this guide moves to the newer release, so set `ZISK_RELEASE` in `.env` to build for another, and the image name carries it, and the two public sources, `rome-zk-evm` at `ROME_ZK_TAG` and `rome-zk-guest` at `ROME_ZK_GUEST_TAG`, from
`.env`; set `ROME_ZK_TAG=v0.2.2` as shown below; the guest tag defaults to `v0.2.0`), builds the guest for `rendered/genesis.json` inside it and
writes `rendered/guest/<sha256>.elf` and `rendered/guest/vkey.json`. Two machines get the same sha256 for the same
genesis. The command passes the release to the image as a build argument (`ZISK_RELEASE`) with its toolchain tag, and refuses a release with no pin file (`ZiskReleaseUnknown`); each release's download URLs, sha256 values,
Rust toolchain tag and key-set root are pinned in `guest-build/zisk/<release>.env`, which has files for 1.2.0-alpha and
1.3.1-alpha. `vkey.json` names the release (`zisk`) and its scheme number (`scheme`: 1 for 1.2.0-alpha, 2 for
1.3.1-alpha), and a proving-key directory of another release is refused (`ProvingKeyMismatch`, naming both releases).
The release is the image's own, so a guest locked to the ZisK crates of another release is refused when the image is built
(`GuestZiskMismatch`), and a `cargo-zisk` of another release or commit when it runs (`ZiskToolchainMismatch`).
It takes about 8 minutes the first time and about 6 minutes once the image exists. It refuses by name
a genesis whose chain id differs from `rendered/chain-id.env` (`ChainIdMismatch`) and a genesis with more than
one funded account (`GenesisFundedAccountLimit`). The programVK in `vkey.json` is computed with the ZisK proving
keys, which the command mounts read-only from `ZISK_HOME` (about 36 GB of memory, about 40 seconds); without
`ZISK_HOME`, or with `--skip-program-vk`, `vkey.json` has no programVK, the command says `ProgramVkNotComputed`,
and the prover refuses the file until the programVK is added. Set `ELF_DIR` to `rendered/guest` and `VKEY_JSON`
to `rendered/guest/vkey.json`. Without that guest, your chain cannot post a root.

The prover needs one NVIDIA GPU with more than 30 GB of memory and the ZisK proving keys on the host: about 26 GB to
download, about 81 GB once installed (measured on a CPU host; a GPU host generates its own files, not measured yet).
The final proof step needs about 30 GB of GPU memory; a 24 GB card is not enough.
[Setting up a prover host](../../docs/PROVER-HOST.md) installs ZisK, its proving keys and the GPU runtime on such a
machine. To turn the prover on, set `PROVER=on`
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
ZisK 1.3.1-alpha, and a mismatch stops the prover by name (`KeysShaMismatch`). The `*.consttree` files under
`provingKey` are not part of the hash: ziskup generates them on the host at install. Only to pin a key set of your own,
on purpose, run this once on the host and keep the result:

```
ZISK_HOME=<the directory you downloaded the keys to> MANIFEST=./prover/keys.sha256 bash ./prover/check-keys.sh --write
```

The hash sorts file names bytewise (the script sets `LC_ALL=C` itself), so a pin made on a host with any `LANG` still matches inside the container.

To keep the manifest somewhere else, set `PROVER_KEYS_MANIFEST` in `.env` to its path.

## The node image

Set `ROME_ZK_TAG=v0.2.2` in `.env` to run the published node image
`ghcr.io/rome-protocol/rome-zk-evm:v0.2.2`. Public tags are
pinned to the commit of the source export they were built from, so a tag names exactly one tree and never
moves. `./rollup up` stops with `ImageTagNotSet` until you set the tag. To run an
image you built from this tree yourself, set `ROME_ZK_IMAGE` to its full reference instead.

## Exits

The genesis carries the exit portal, the contract where a withdrawal starts, at `0x4200000000000000000000000000000000000016`.
New chains start with no exit portal configured and an exit cap of zero, so exits are off. The chain authority
can run `./rollup exit-config propose` to set any of the portal (`--exit-portal 0x...`), the bridge program
(`--bridge-program`), the cap (`--exit-cap`) and the poster bond (`--poster-bond`), with either `--activation-slot N` or
`--activation-delay-slots N`. It prints what it would send until you add `--confirm`. Once the activation slot has
passed, which is at least one 172,800-slot challenge window away, `./rollup exit-config activate --confirm` makes it
current, and `./rollup exit-config show` prints the current values and anything pending. `./rollup check` shows the same
exit configuration. A final root and a funded vault are also needed to release an exit; once an exit is proved,
`./rollup release-exit --message-hash 0x... --confirm` pays it out of the vault. On devnet, exits are paid from the shared zk-bridge program
listed in `programs.devnet.json`, out of your chain's own vault.

## Your chain's vault

Each chain has its own vault in the shared zk-bridge program, keyed by the settlement program and the chain id.
Only the chain authority (your payer key) can create it, and only after `register`. A vault holds one token mint,
chosen when it is created. A backed balance needs the wrapped SOL mint,
`So11111111111111111111111111111111111111112`, which has 9 decimals.

Run these from this directory after `init`; they take the chain id, the Solana RPC URL, your payer key and the program
addresses from `rendered/` and `.env`. `vault init` and `vault fund` print what they would send and send nothing until you add
`--confirm`.

```sh
./rollup vault init                  # dry run; wrapped SOL, 9 decimals by default
./rollup vault init --confirm
```

`init` takes `--mint M` and `--mint-decimals N` for another mint.

`fund` moves the amount from the funding key's wrapped SOL token account (its associated token account for that
mint), so wrap the SOL first, for example with `spl-token wrap`, which comes with the Solana CLI. For a backed
balance, `--amount` is the lamport amount `init` printed. Anyone can fund a vault.

```sh
./rollup vault fund --amount <lamports>
./rollup vault fund --amount <lamports> --confirm
```

`./rollup vault show` prints the vault and its balance.

## Key files

Both are paths in `.env`, never the keys themselves. The containers run as uid 999, so the files must be
readable by that user. From this directory, create them with the Solana CLI and openssl:

```sh
mkdir -p keys
solana-keygen new -o keys/payer.json
openssl rand -hex 32 > keys/sequencer.key
sudo chgrp 999 keys/payer.json keys/sequencer.key && chmod 640 keys/payer.json keys/sequencer.key
```

`solana-keygen` asks for an optional passphrase (press Enter for none; `--no-bip39-passphrase` skips the question),
then prints the new public key and a recovery phrase. Keep the phrase private. It also refuses to overwrite an existing
file.

Fund the payer with about 7 devnet SOL, for example from [Solana's faucet](https://faucet.solana.com).
The host also reads the payer key during `init` and `register`. `.gitignore` in this directory excludes `keys/`.

- `SEQUENCER_KEY_PATH`: the sequencer's signing key, 64 hex characters in a file.
- `PAYER_KEYPAIR_PATH`: a Solana keypair (JSON) funded with SOL. It pays Solana fees and rent and is the chain authority.

## Running the tests

```
for t in deploy/rollup/tests/*.sh; do bash "$t"; done
```

They use fixtures and stubs only: no network, no Docker, no GPU.
