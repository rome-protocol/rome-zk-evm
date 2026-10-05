# Run your own rollup on Solana devnet

This guide runs your chain on Rome's shared settlement programs on public Solana devnet.
You run the node and pay its Solana costs. You do not deploy your own settlement programs.
Registration is permissionless. Finalizing roots needs a prover and a verification key registered by Rome.

## What is live on devnet

The shared zk-inbox, zk-settlement, veritas and zk-bridge programs are live on Solana devnet. The deposit
instructions and the ZisK 1.3.1-alpha verifier became live together. Their addresses are in
[`programs.devnet.json`](../deploy/rollup/programs.devnet.json). Build from the release tag below; `main` can
be ahead of these programs.

Permissionless registration is on, with a 5 SOL registration deposit. Posting a proved root costs 0.001 SOL, with no gas-based fee (0 bps).
A chain that has posted no root can be reclaimed 6,480,000 slots after registration, about 30 days. These settings are in the settlement global config account `4F1ajEqgBK6tFc1RkrjbXghRJzVzFH2dg4mHsegpJf3y`. Rome holds the program upgrade
authority and registers verification keys. The node image is `ghcr.io/rome-protocol/rome-zk-evm:v0.3.0`.
Read the [devnet trust model](TRUST-MODEL.md) for the powers and limits of Rome's keys and your chain authority.

## What you need

- An x86-64 Linux machine with bash 4 or newer, Git, Docker Engine 28 or newer with the Compose plugin,
  curl, jq, openssl, the Solana CLI and Python 3.11 or newer. You do not need Rust or a compiler: every command that
  talks to Solana runs inside the node image.
- A Solana devnet RPC endpoint. A provider endpoint is more reliable under load.
- A Solana payer keypair in a JSON file, funded with about 7 SOL on devnet. It is also your chain authority.
- A sequencer signing key: 64 hex characters in a file. Both key files must be readable by container user 999.
- An EVM address you control for the fee recipient (and, if you declare one, the backed balance).

The steps below start the node without a prover. A full settlement setup also needs one NVIDIA GPU
with more than 30 GB of memory and the ZisK 1.3.1-alpha proving keys on the host. The final proof step needs
about 30 GB of GPU memory; a 24 GB card is not enough. You also need a guest built for your chain's genesis.
[Setting up a prover host](PROVER-HOST.md) covers the GPU machine.

## From clone to a running node

### 1. Clone and prepare the settings

```sh
git clone --branch v0.3.0 https://github.com/rome-protocol/solana-zk-evm.git
cd solana-zk-evm/deploy/rollup
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

`solana-keygen` asks for an optional passphrase (press Enter for none) and prints the new public key and a recovery
phrase. Keep the phrase private.

Fund `keys/payer.json` with about 7 devnet SOL. You can use a devnet faucet such as
[Solana's faucet](https://faucet.solana.com). The containers run as uid 999, and `init` and `register`
also read the payer key on the host. `deploy/rollup/.gitignore` excludes `keys/` from Git.

Edit `.env`:

- Set `SOLANA_RPC_URL` to your Solana devnet RPC endpoint.
- Keep `PROGRAMS_JSON=programs.devnet.json`.
- Set `PAYER_KEYPAIR_PATH` and `SEQUENCER_KEY_PATH` to your key files. These are paths, not key values.
- Set `ROME_ZK_TAG=v0.3.0`, `ROME_ZK_GUEST_TAG=v0.3.0` and `ZISK_RELEASE=1.3.1-alpha`.
- Keep `RPC_BIND=127.0.0.1` for local access and leave `PROVER` unset for this first run.

A new chain's genesis has no balances, and you do not need to set anything for that. A genesis that mints coins could
never take deposits safely, because its holder could exit coins that other people's deposits paid for, and the genesis
cannot change after you register. So a chain starts with zero balances, and `init` refuses a `chain.toml` that sets
`genesis.funded_address`. L2 gas comes from deposits after the chain has a proved root and its deposit queue is ready.
Until then a chain with no backed balance cannot send a transaction. You can still register it and start the services.

If you need gas before the deposit queue is ready, you can declare one backed balance, with two keys under `[genesis]`:
`backed_address` (an address you control) and `backed_balance_lamports` (the amount in lamports, for example
`1_500_000_000` for 1.5 SOL). Your chain gets that amount at genesis, 1 lamport as 1 gwei. You then lock the same
amount in your chain's vault with the bridge program's `Fund`, and Rome checks the vault before it registers your
verification key. `init` prints the exact amount to lock. After `register`, your chain authority creates the
chain's vault for wrapped SOL with
`./rollup vault init --confirm`, then locks the amount with `./rollup vault fund --amount <lamports> --confirm`. The
deployment guide's [Your chain's vault](../deploy/rollup/README.md#your-chains-vault) has the details.

Set `genesis.fee_recipient` to an address you control as well. It receives the priority fees (tips) of the chain's
transactions; the base fee is burned, as on Ethereum. Like the rest of the genesis, it cannot be changed after you
register. `init` refuses it if it is missing, malformed, zero, a precompile address or the exit portal's address.
Do not add a chain id. The settlement program derives it from your payer key and that key's
registration count; `init` reads it from Solana. The id is a number between 2^32 and 2^53 - 1, and MetaMask cannot add
a chain whose id is above 4503599627370476, which is about half of them. If yours is above it, `init` stops with
`ChainIdNotWalletSafe` before anything is sent. Create another payer key in a new file, for example
`solana-keygen new -o keys/payer2.json`, give it the same `chgrp 999` and `chmod 640` as before, set
`PAYER_KEYPAIR_PATH` to it and run `./rollup init` again. A different key gets a different id. `init` only reads from
Solana, so you can check the id before you fund the key.

### 2. Read the chain id and render the configs

```sh
./rollup init
```

It runs `rome-zk-ops` from the node image to read your chain id from Solana (the first run pulls the image, so
`ROME_ZK_TAG` must be set) and writes the genesis and service configs to `rendered/`. Its summary starts with `rendered under`
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

`check` prints `PASS`, `FAIL`, `WARN` or `SKIP` for each item and ends with
`== rollup check :: ALL PASS ==` when no item failed. A failure starts with its name. Besides the services, heads and
roots, it checks:

- **batches finalized**: blocks were sealed and the batcher finalized a batch over the window. A stuck batcher fails
  with `BatchesNotFinalizing`. Run `check` every few minutes; the first run only records a sample.
- **payer balance**: the payer holds at least `PAYER_FLOOR_LAMPORTS` (1 SOL by default), or `PayerBelowFloor`.
- **reclaim deadline**: a chain that has not posted a root can be reclaimed by anyone after the window. `check` fails
  with `ReclaimDeadlineNear` below `RECLAIM_MARGIN_SLOTS` (about 12 hours), so a timer acts while there is still time to
  post a root, and with `ReclaimDeadlinePassed` once the window is over.
- **verification key** (prover on): fails with `VkeyNotActive` until Rome has registered a key for your chain.
- **exit config**: shows the exit portal, the bridge program and any pending change.

With no prover configured, settlement lag, prover lag and the verification key are skipped. The deployment guide's
[What `check` looks at](../deploy/rollup/README.md#what-check-looks-at) lists every item and setting.

The same guide lists the other commands: `./rollup status` (services and what Solana says about the chain),
`./rollup logs`, `./rollup down`, `./rollup refund-deposit`, `./rollup exit-config`, `./rollup vault`,
`./rollup deposit-queue`, `./rollup deposit`, `./rollup close-deposit`, `./rollup release-exit` and
`./rollup migrate`. Those that send a transaction print it and send nothing until you add
`--confirm`.

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

Once `./rollup guest-build` has finished (described later in this section), send your request with the
[verification key request form](https://github.com/rome-protocol/solana-zk-evm/issues/new?template=vkey-request.yml).
Have these ready:

- your chain id, the `CHAIN_ID` line of `rendered/chain-id.env`;
- the public key of your chain authority (the `AUTHORITY` line of the same file), and the signature of the transaction that registered your chain;
- the guest tag you built at;
- the contents of `rendered/guest/vkey.json`, which holds the ELF's sha256 (`elf_sha256`), the genesis file's
  sha256 (`genesis_sha256`), the `programVK`, and the ZisK release in `zisk` and `scheme`
  (for this release, `1.3.1-alpha` and `2`);
- `rendered/genesis.json`, pasted or attached;
- if your genesis declares a balance, the vault that backs it, already funded;
- a `./rollup check` that passes apart from the items that wait for the key (verification key, and deposit-capable
  key).

Rome registers a complete request within two business days. The time matters. The settlement program lets a chain
that has no proved root be reclaimed 6,480,000 slots after its registration, about 30 days at 400 ms slots, and that
count starts when you register the chain, not when the key is registered. A chain cannot prove a root without a key,
so every day spent waiting is a day off that window.

Rome rebuilds the guest from the tag and your genesis, checks that the chain id, the genesis, the ELF's sha256 and
the programVK are the ones you sent, registers the key under that ZisK release, and replies on your issue with the
transaction signature. The key becomes active at the slot the registration sets. Run `./rollup check` after that slot:
the deposit-capable key item passes, and with the prover on (`PROVER=on`, set up below) so does the verification key
item. An incomplete request gets one reply that says what is
missing, and the two days start when it is complete.

To move an existing chain to another ZisK release, see [ZisK releases](ZISK-RELEASES.md).

The batch guest source is [rome-protocol/rome-zk-guest](https://github.com/rome-protocol/rome-zk-guest),
tag `v0.3.0`. `./rollup guest-build` clones the guest sources itself.

Build the guest for your chain with one command:

```sh
./rollup guest-build
```

The guest contains your genesis, so every chain has its own guest program. The command builds an image with the
ZisK toolchain, clones the two public sources at the tags this guide names, builds the guest from your
`rendered/genesis.json` and checks that it carries your chain id from `rendered/chain-id.env`. It writes
`rendered/guest/<sha256>.elf` and `rendered/guest/vkey.json`, and prints the ELF's sha256. Two machines that run
it for the same genesis get the same sha256.

It refuses by name a genesis whose `chainId` differs from your chain's id (`ChainIdMismatch`) and a genesis
that gives more than one account a balance (`GenesisFundedAccountLimit`).

`vkey.json` holds the ELF's sha256, your chain id, the ZisK release and the programVK, the verification key of this guest.
The programVK needs the ZisK proving keys and about 36 GB of memory, about 40 seconds. Set `ZISK_HOME` in
`.env` to the directory that holds them (see the [prover host setup](PROVER-HOST.md)) and the command computes it. Without
`ZISK_HOME`, or with `--skip-program-vk`, it writes `vkey.json` without the programVK and says
`ProgramVkNotComputed`. The prover refuses that file: add the programVK from a machine with the keys, or ask
Rome for it, before you start the prover.

Point `ELF_DIR` at `rendered/guest` and `VKEY_JSON` at `rendered/guest/vkey.json` in `.env`.

Once your key is registered and you have the matching guest and proving keys, follow the
[prover setup](../deploy/rollup/README.md#the-prover-is-required-for-settlement).
It covers `PROVER=on`, `VKEY_JSON`, `ELF_DIR`, `ZISK_HOME` and the proving-key hash manifest.

## Open deposits on your chain

First post and prove a root with the registered ZisK 1.3.1-alpha key. A chain with no balances sends no transactions, and while `empty_block_interval_secs` under `[profile]` in `chain.toml` is 0, the default, its sequencer seals no blocks, so there is nothing to prove. Set it to a number of seconds such as `60`, then run `./rollup init` and `./rollup up`, or declare a backed balance at genesis. Once the first root is proved you can set it back to 0 if you like, running `./rollup init` and `./rollup up` again. Then create a wrapped SOL vault with
`./rollup vault init --confirm`. Your chain's exit configuration must name the bridge program in
`programs.devnet.json`. Use `./rollup exit-config propose --bridge-program <address>
--activation-delay-slots N --confirm`, with `N` a little more than one challenge window, then
`./rollup exit-config activate --confirm` after its activation slot. `./rollup exit-config show` shows the
current bridge. The queue command also requires at least one posted root, a registered chain and its vault.

```sh
./rollup deposit-queue init
./rollup deposit-queue init --confirm
./rollup deposit-queue show
```

The chain authority runs `init`. The default inclusion deadline is 12 hours. A batch can take
up to 256 deposits, with up to 4 per block. The minimum is 1,000,000 lamports of wrapped SOL (0.001 SOL).
Each deposit pays a 100,000 lamport (0.0001 SOL) fee to the chain authority's payer key. The depositor also pays the Solana transaction fee and the rent for its deposit record. `init` accepts `--deadline-secs`, `--max-per-batch`, `--max-per-block`, `--min-amount`,
`--fee-lamports` and `--fee-recipient`. The deadline must be 1 to 24 hours, the per-batch cap at most 256,
the per-block cap between 1 and the per-batch cap, the minimum at least one raw token unit, and the fee at
most 0.01 SOL. The fee recipient must be a Solana account that holds at least the rent-exempt minimum for an empty account and is neither executable nor a sysvar.
The per-block cap times the chain's blocks per batch must fit the per-batch cap.

To change a parameter, run `./rollup deposit-queue propose --min-amount N --confirm`, then
`./rollup deposit-queue activate --confirm` once the activation slot has passed. The proposal can name any
of the `init` parameter flags. It can also name `--activation-slot N` or `--activation-delay-slots N`.
The activation must be between one and two challenge windows away. If you omit both flags, the command
chooses one challenge window plus a margin. A new proposal replaces a pending one. Use
`./rollup deposit-queue show` to see the live and pending settings, queue count, backlog and oldest wait.

Run `./rollup check` before opening the queue to compare the active key with your deposit-capable guest.
Keep running it afterward to watch deposit caps, vault backing, bridge configuration, the sequencer's
deposit settings, the deposit cursor, the oldest waiting deposit and the backlog. See
[What `check` looks at](../deploy/rollup/README.md#what-check-looks-at).

## Deposit SOL

Use a Solana keypair you control, saved as `keys/depositor.json` and readable by container user 999.
`--wrap-sol` wraps the amount in the same transaction as the deposit;
the chain's vault must hold wrapped SOL. The example deposits the default minimum. Replace the recipient
with your 20-byte address on this chain.

```sh
./rollup deposit --amount 1000000 --recipient 0x0123456789abcdef0123456789abcdef01234567 --keypair keys/depositor.json --wrap-sol
./rollup deposit --amount 1000000 --recipient 0x0123456789abcdef0123456789abcdef01234567 --keypair keys/depositor.json --wrap-sol --confirm
```

The first command is a dry run. The depositor pays the queue's fee, transaction fee and deposit-record
rent. The command prints the record index. A deposit record stores that index, the depositor, L2 recipient,
amount, enqueue time and queue hash. The sequencer reads finalized Solana deposits and credits them in
order in new L2 blocks. The L2 balance appears when the crediting block is sealed and served by the RPC.
That credit becomes final only when a proof settles its batch on Solana.

After the crediting batch is final, its batch authority (the payer key) must send `CloseBatch` for that inbox batch.
This advances the cursor that lets deposit records close. Nothing in this release sends it (see
[Known limits](#known-limits)). Once it has been sent, close the record by its printed index:

```sh
./rollup close-deposit --index N --confirm
```

Anyone can pay for this transaction. The record's rent always returns to the depositor. The wrapper uses
the chain payer key from `.env` to send it.

## Costs

Budget about 7 SOL on the payer to start. The 5 SOL deposit is locked at registration, and `./rollup check`
wants at least 1 SOL left on the payer for the batcher's transactions and rent (`PAYER_FLOOR_LAMPORTS`). It is refundable
to the chain authority once the chain has a final root or has posted ten roots. If the chain is reclaimed
first, the deposit goes to the treasury. For your chain, the 0.001 SOL settlement fee is charged only
when a proved root is posted, with 0 bps added. The payer also covers Solana transaction fees and
rent on inbox batch and chunk accounts. With a prover, it pays rent for each posted root's pending
account too. For a posted batch, inbox rent can be reclaimed only after a root covering it is final.
Nothing in `deploy/rollup` closes posted inbox accounts. The prover closes pending accounts only when
`close_pending_after_batches` is set. Plan for rent to keep accumulating, with or without a prover.
Watch the payer balance on a busy chain.
Your node, RPC service and prover have their own running costs.

## Known limits

- Never send `AbandonBatch` by hand for a batch that has not settled: it halts the chain. Settlement posts
  exactly the next batch id and needs that id's batch finalized, and an abandoned id can be neither reopened
  nor skipped. If the batcher stops mid-batch, just start it again: it finishes the open batch under the same
  id. If it stops with `ResumeImpossible`, rerun it with the build and config that opened the batch.
- The guest build needs Docker and, for the programVK, the ZisK proving keys and about 36 GB of memory on the machine that runs `./rollup guest-build`.
- A new chain has no gas until it receives a deposit or starts with a backed balance. Opening deposits needs a proved root, a vault, a bridge exit configuration and a deposit queue. A chain with no balances seals no blocks while `empty_block_interval_secs` is 0, so set it above 0 to get blocks for that first proved root.
- The prover needs one NVIDIA GPU with more than 30 GB of memory. The final proof step needs about 30 GB
  of GPU memory; a 24 GB card is not enough. See [prover host setup](PROVER-HOST.md) for disk needs.
- Withdrawals need a final root and a proved exit. The exit prover is a separate service and is not in the
  node image or this deployment folder.
- Nothing in this release sends the inbox's `CloseBatch`, and `./rollup` has no `close-batch` command. The batch authority, which is your payer key, must build and sign it itself after the batch's root is final; the inbox client library's `close_batch_ix` builds the instruction. Until a close service ships, batch, chunk and deposit-record rent stays locked: `close-deposit` refuses a record (`DepositNotFinal`) until the crediting batch has been closed with `CloseBatch` by its batch authority.
- Exits are off on a new chain: it has no exit portal configured and its exit cap is zero. The chain
  authority can run `./rollup exit-config propose --exit-portal 0x4200000000000000000000000000000000000016
  --bridge-program 27TbMDUyVynpFpqeKygpUMcDzWKHfW4k9aRN5yCysLEQ --exit-cap N
  --activation-delay-slots 175000 --confirm`, where `N` is the most that may exit in one challenge window, in gwei
  (1 gwei is 1 lamport of wrapped SOL). The program refuses an activation slot less than one 172,800-slot
  challenge window away (`ActivationTooSoon`), and the delay counts from the slot the command reads before it
  sends, so give a little more than the window. Once that slot has passed, run `./rollup exit-config activate
  --confirm`, then `./rollup exit-config show`. The chain authority creates its vault with `./rollup vault init
  --confirm`, wraps the SOL to lock with `spl-token wrap`, funds the vault with `./rollup vault fund --amount N
  --confirm` (in lamports for wrapped SOL), and reads it with `./rollup vault show`. The vault is keyed by the
  settlement program and your chain id.
