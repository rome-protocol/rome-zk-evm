# Run your own rollup on Solana devnet

This guide runs your chain on Rome's shared settlement programs on public Solana devnet.
You run the node and pay its Solana costs. You do not deploy your own settlement programs.
Registration is permissionless. Finalizing roots needs a prover and a verification key registered by Rome.

## What is live on devnet

Rome verified the program addresses and settlement settings on chain. The program addresses are also in
[`deploy/rollup/programs.devnet.json`](../deploy/rollup/programs.devnet.json).

| Address or setting | Value |
| --- | --- |
| Cluster | Solana devnet |
| zk-inbox | `28948c2Qt1QtG2XJA823ytzCfj7U5cNNm3FuM2KoXsAa` |
| zk-settlement | `8anSjJZu5vgfNbESPLoKudVBNEraDZASwKJnkDLTCaGo` |
| veritas | `2cLGd9FKC7TiZrEHCvpw291AwXNcGgP9nDT4W3PHLe5k` |
| Settlement global config | `4F1ajEqgBK6tFc1RkrjbXghRJzVzFH2dg4mHsegpJf3y` |
| Permissionless registration | On |
| Registration deposit | 5 SOL |
| Settlement fee | 0.001 SOL when a proved root is posted; 0 bps |
| Reclaim window | 6,480,000 slots, about 30 days at 400 ms per slot. If no root has been posted, anyone can reclaim the chain; its accounts close and the batcher stops. |
| Registry authority | `7CsvZgCML2C7i4f1Qd6Au3cxonB4c8uAWn3NmMmU9MDk` — Rome's key that registers verification keys |
| Treasury | `2H8bMM3AUTU6RZap3zyFU5coNUazo6z5Xg7ighfCTJ3q` |
| Program upgrade authority | `2H8bMM3AUTU6RZap3zyFU5coNUazo6z5Xg7ighfCTJ3q` — held by Rome |
| Node image | `ghcr.io/rome-protocol/rome-zk-evm:v0.1.1` |

## What you need

- An x86-64 Linux machine with bash 4 or newer, Git, Docker Engine 28 or newer with the Compose plugin,
  curl, jq, openssl, the Solana CLI, Python 3.11 or newer, a C compiler, and rustup with cargo. The repository pins
  the Rust toolchain; rustup installs it on the first build. On Debian or Ubuntu, install the compiler
  with `sudo apt install build-essential`.
- A Solana devnet RPC endpoint. A provider endpoint is more reliable under load.
- A Solana payer keypair in a JSON file, funded with about 6 SOL on devnet. It is also your chain authority.
- A sequencer signing key: 64 hex characters in a file. Both key files must be readable by container user 999.
- An EVM address you control for the fee recipient (and, if you declare one, the backed balance).

The steps below start the node without a prover. A full settlement setup also needs one NVIDIA GPU
with more than 30 GB of memory and about 55 GB of proving keys on the host. The final proof step needs
about 30 GB of GPU memory; a 24 GB card is not enough. You also need a guest built for your chain's genesis.

## From clone to a running node

### 1. Clone and prepare the settings

```sh
git clone --branch v0.1.1 https://github.com/rome-protocol/rome-zk-evm.git
cd rome-zk-evm/deploy/rollup
cp .env.example .env
cp chain.toml.example chain.toml
```

Git prints its clone progress. The other commands normally print nothing.
Run the remaining `./rollup` commands from this directory.

Create the key files here with the Solana CLI and openssl:

```sh
mkdir -p keys
solana-keygen new -o keys/payer.json
openssl rand -hex 32 > keys/sequencer.key
sudo chgrp 999 keys/payer.json keys/sequencer.key && chmod 640 keys/payer.json keys/sequencer.key
```

Fund `keys/payer.json` with about 6 devnet SOL. You can use a devnet faucet such as
[Solana's faucet](https://faucet.solana.com). The containers run as uid 999, and `init` and `register`
also read the payer key on the host. `deploy/rollup/.gitignore` excludes `keys/` from Git.

Edit `.env`:

- Set `SOLANA_RPC_URL` to your Solana devnet RPC endpoint.
- Keep `PROGRAMS_JSON=programs.devnet.json`.
- Set `PAYER_KEYPAIR_PATH` and `SEQUENCER_KEY_PATH` to your key files. These are paths, not key values.
- Set `ROME_ZK_TAG=v0.1.1` to use the node image above.
- Keep `RPC_BIND=127.0.0.1` for local access and leave `PROVER` unset for this first run.

A new chain's genesis has no balances, and you do not need to set anything for that. The older
`genesis.funded_address` key is gone: it minted coins to one address, and a genesis that mints coins can never take
deposits safely, because its holder could exit coins that other people's deposits paid for. The genesis cannot
change after you register, so a chain starts with zero balances. `init` refuses a `chain.toml` that still has
`funded_address`. L2 gas comes from deposits once they ship. Until then your chain has no gas and cannot send a
transaction, so there is nothing for it to carry yet. You can still register it and start the services.

If you need gas before deposits are available, you can declare one backed balance, with two keys under `[genesis]`:
`backed_address` (an address you control) and `backed_balance_lamports` (the amount in lamports, for example
`1_500_000_000` for 1.5 SOL). Your chain gets that amount at genesis, 1 lamport as 1 gwei. You then lock the same
amount in your chain's vault with the bridge program's `Fund`, and Rome checks the vault before it registers your
verification key. `init` prints the exact amount to lock. The bridge program is not deployed on devnet yet, so a backed
balance cannot be locked on devnet today.

Set `genesis.fee_recipient` to an address you control as well. It receives the priority fees (tips) of the chain's
transactions; the base fee is burned, as on Ethereum. Like the rest of the genesis, it cannot be changed after you
register. `init` refuses it if it is missing, malformed, zero, a precompile address or the exit portal's address.
Do not add a chain id. The settlement program derives it from your payer key and that key's
registration count; `init` reads it from Solana. The id is a number between 2^32 and 2^53 - 1, and MetaMask cannot add
a chain whose id is above 4503599627370476, which is about half of them. If yours is above it, `init` stops with
`ChainIdNotWalletSafe` before anything is sent. Create a new payer key with `solana-keygen new -o keys/payer.json`,
give it the same `chgrp 999` and `chmod 640` as before, and run `./rollup init` again. `init` only reads from Solana,
so you can check the id before you fund the key.

### 2. Read the chain id and render the configs

```sh
./rollup init
```

The first run compiles the client, which can take a few minutes. It reads your chain id from Solana
and writes the genesis and service configs to `rendered/`. Its summary starts with `rendered under`
and includes the authority, nonce, chain id, gas limit, blocks per batch and cluster.
Your chain id is saved in `rendered/chain-id.env`.

Keep `rendered/` out of version control: it contains secrets and may contain your RPC credentials.

### 3. Inspect registration, then register

```sh
./rollup register --dry-run
```

This prints the chain id and the commands for registration, the batch cursor and the account addresses.
It makes no network calls and starts nothing.

Until your chain posts its first root, anyone can reclaim it about 30 days after registration.
That closes its root, registry and chain configuration accounts. Their SOL, including the deposit,
goes to the treasury, and the batcher stops.

```sh
./rollup register --confirm
```

This registers your chain through the permissionless path, locks the 5 SOL deposit and creates the batch cursor.
It starts the sequencer if needed so it can read the genesis block. It prints the registration output
and ends with `registered; wrote` followed by the path to `rendered/pdas.env` and the cursor and root addresses.
It sends no verification key.

If it stops after registration, you can run the same command again. It detects the registered chain
and finishes the cursor step without registering another chain.

### 4. Start the services and check them

```sh
./rollup up
./rollup check
```

`up` prints Docker Compose startup output. It starts the sequencer, reth-verifier, derive and batcher.
The sequencer's HTTP RPC is at `http://127.0.0.1:8545`; its WebSocket port is 8546.

`check` prints `PASS`, `FAIL` or `SKIP` for each item and ends with
`== rollup check :: ALL PASS ==` when the checks pass.
With no prover configured, settlement lag and prover lag are skipped.

For remote RPC access and other settings, see the [deployment guide](../deploy/rollup/README.md).

### Moving to a new node image

From the repository root, run `git fetch --tags` and check out the source tag that matches the new image
with `git checkout <new tag>`. Set `ROME_ZK_TAG` in `deploy/rollup/.env` to that tag. Then, from
`deploy/rollup`, run `./rollup init` and `./rollup up`. `init` copies the tag into `rendered/compose.env`,
which `up` reads. It keeps your chain id and stops with `GenesisDrift` if the new source would change
your genesis.

## What works without a prover

The sequencer accepts transactions and serves the chain's RPC. The batcher posts batches to the Solana inbox.
Derive reads that inbox and rebuilds the chain through reth-verifier.

These chains are proved-only. A root becomes final on Solana only with a ZisK proof checked against
the chain's registered verification key. Without that proof, no root is final, including the genesis root,
and no withdrawal can complete. A passing `roots` check means the root account is readable;
it does not mean the chain has finalized a root.

## Getting your verification key registered

When you have a guest built for your chain's genesis, [open an issue on rome-protocol/rome-zk-evm](https://github.com/rome-protocol/rome-zk-evm/issues).
Include your chain id from `rendered/chain-id.env` and the guest ELF's sha256. Rome registers the
matching verification key through the registry authority listed above.

The batch guest source is [rome-protocol/rome-zk-guest](https://github.com/rome-protocol/rome-zk-guest),
tag `v0.1.1`. From the root of your `rome-zk-evm` checkout, clone it into `.fork/` and initialize
its submodules:

```sh
git clone --branch v0.1.1 https://github.com/rome-protocol/rome-zk-guest.git .fork
cd .fork && git submodule update --init --recursive
```

Git prints submodule checkout progress as needed. Building the guest for your `rendered/genesis.json`
is not automated yet. Without that guest, your chain cannot post a root.

Once your key is registered and you have the matching guest and proving keys, follow the
[prover setup](../deploy/rollup/README.md#the-prover-is-required-for-settlement).
It covers `PROVER=on`, `VKEY_JSON`, `ELF_DIR`, `ZISK_HOME` and the proving-key hash manifest.

## Costs

Budget about 6 SOL on the payer to start. The 5 SOL deposit is locked at registration. It is refundable
to the chain authority once the chain has a final root or has posted ten roots. If the chain is reclaimed
first, the deposit goes to the treasury. For your chain, the 0.001 SOL settlement fee is charged only
when a proved root is posted, with 0 bps added. Without a prover, Solana transaction fees and rent still
use the payer's SOL. Each batch the batcher posts creates inbox accounts, each locking at least about
0.002 SOL in rent until a root covering the batch is final. Watch the payer balance on a busy chain.
Your node, RPC service and prover have their own running costs.

## Known limits

- Building a guest for your chain's genesis is not automated yet.
- A new chain has no gas: its genesis has no balances, and deposits are not available yet. Until they are, it cannot send a transaction.
- The prover needs one NVIDIA GPU with more than 30 GB of memory and about 55 GB of proving keys on the host.
  The final proof step needs about 30 GB of GPU memory; a 24 GB card is not enough.
- Exits are off on a new chain: it has no exit portal configured and its exit cap is zero. The chain
  authority can use the settlement client's `governance` example to send `propose-exit-config`.
  After at least one 172,800-slot challenge window, anyone can send `activate-exit-config`. Releasing an
  exit also needs a bridge program, which the devnet program set does not include yet.
- The devnet programs are upgradeable. Rome holds their upgrade authority.
