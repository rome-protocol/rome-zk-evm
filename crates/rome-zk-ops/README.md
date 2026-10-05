# rome-zk-ops

One binary, `rome-zk-ops`, for the things an operator does to a chain's settlement accounts after the node is
running. Registering a chain, getting its deposit back, changing its exit configuration, the one-time
migrations, the bridge vault and releasing a proved exit all live here, so an operator learns one command line
instead of a handful of cargo examples.

```
rome-zk-ops [--rpc-url URL] [--confirm | --dry-run | --offline] <command>
```

| Command | What it does |
| --- | --- |
| `chain-id --settlement P (--keypair F \| --authority K)` | Prints the chain id the next permissionless registration of that authority will get, as `authority=`, `nonce=` and `chain_id=` lines. Sends nothing. |
| `register --keypair F --settlement P --inbox P --evm-rpc URL [--reserved ...]` | Registers a chain. Permissionless by default (the id comes from the authority's nonce and the configured deposit is locked). `--reserved` needs `--chain-id`, `--registry-keypair` and both verifier-key files. |
| `chain-status --settlement P --chain-id N` | Prints `key=value` lines from the chain's settlement accounts: authority, current slot, batch heads, posted count, the reclaim deadline and the slots left (`none` once the chain has posted), the deposit, and whether a verification key for the proving layout is active now (or the slot a pending one activates at). Read-only; a failed read is refused by name. |
| `vkey register --settlement P --chain-id N --genesis F --zisk R --guest-tag T --elf-sha256 H --program-vk H --proving-key-dir D [--anchor-proving-key-dir D] --registry-keypair F --payer-keypair F (--activation-slot N \| --activation-delay-slots N) [--anchor-guest-tag T] [--anchor-zisk R] [--anchor-evm-tag T] [--bridge-program ID] [--move-pending-activation]` | Rome's checked registration of a chain's verification key, for the registry authority. It reads the chain's root and refuses `PortalNotCanonical`, `ChainIdMismatch`, `GenesisMismatch`, `ElfMismatch`, `VkeyMismatch`, a vault that does not hold the genesis balance, `GenesisUnanchored`, `VkeyUnderOtherZiskVersion`, `VkeyAlreadyActive` or `VkeyPending` unless what the operator sent survives a rebuild of the guest (see below), then builds `SetRegistryEntry` for the proving layout, with the scheme number of the ZisK release named by `--zisk` (required). A release that is not open is refused first, before any file is read (`ZiskVersionNotOpen`; `ZiskVersionUnknown` for a name the registry does not have). A dry run prints the instruction and the slot it would set; `--confirm` sends it. |
| `vkey show --settlement P --chain-id N` | Prints a chain's registry entries, each with its state (`pending`, `active` or `retired`), layout, curve, scheme, key, activation slot, ZisK release (`zisk=`) and that release's standing (`zisk_status=`), and a `vkey_entry_N_warning=` for a live entry under a release that is closing or withdrawn, after the same `vkey_entries=`, `vkey_active=` and `vkey_pending_activation_slot=` lines `chain-status` prints. Read-only; a failed read is refused by name. |
| `vkey retire-version --settlement P --zisk R --registry-keypair F --payer-keypair F` | Retires, one `SetRegistryEntry` each, every registry entry on every chain of the settlement program that is still live under ZisK release `R`. It finds the registry accounts by scanning the program's accounts for the registry's magic, reads every one before sending anything (an account that does not decode, or a scan that fails, stops the run: `RegistryUndecodable`, `RegistryScanFailed`) and needs the registry authority's key (`NotRegistryAuthority`). A dry run lists the retirements as `retire chain_id=… entry=…` lines and sends nothing; `--confirm` sends one transaction per entry, and a failure says how many were already retired. Retiring is final. Any release the registry names can be retired, whatever its status. It needs the real RPC: `--offline` cannot list accounts. |
| `pdas --inbox P --settlement P --chain-id N` | Prints the addresses of the chain's root, batch cursor, global config, reserved-allow marker and chain config as `name=address` lines. Reads nothing. |
| `refund-deposit --settlement P --chain-id N --keypair F` | Sends the registration deposit back to the chain authority recorded on chain. Anyone funded can pay the fee; the lamports go only to the authority. |
| `exit-config propose ...` | The chain authority proposes an exit portal, bridge program, exit cap or poster bond, with `--activation-slot` or `--activation-delay-slots`. |
| `exit-config activate ...` | Copies a pending proposal into effect once its activation slot has passed. Anyone can send it. |
| `exit-config show --settlement P --chain-id N` | Prints what is in effect, what is pending and whether the proposal can be activated yet. |
| `migrate ... --max-drift-secs N` | `MigrateChainV2`: brings a chain that predates `chain_config` forward. Registry authority only. The drift bound has no default. |
| `init-cursor --keypair F --inbox P --settlement P --chain-id N` | Creates the chain's batch cursor in the inbox program. A cursor that exists is reported and nothing is sent. |
| `vault init --authority-keypair F --mint M --mint-decimals N --settlement P --bridge P --chain-id N` | Creates the bridge vault for a chain. The authority must be the one recorded in the settlement root. A vault that already exists is reported and nothing is sent. |
| `vault fund --payer-keypair F --amount N --settlement P --bridge P --chain-id N` | Moves raw token units from the payer's token account into the vault. Refused by name before any instruction is built if the vault belongs to another settlement program. |
| `vault show --settlement P --bridge P --chain-id N` | Prints the vault's configuration and token balance, or that it is not initialised. Sends nothing. |
| `release-exit --settlement P --bridge P --chain-id N --message-hash 0x... --payer-keypair F` | Reads a proved exit record, creates the recipient's token account if it is missing, and sends `ReleaseExit` in the same transaction. A record that does not exist, or is not proved, is refused by name. |
| `deposit --keypair F --amount N --recipient 0x... --settlement P --bridge P --chain-id N [--wrap-sol]` | Reads the chain's vault and deposit queue, then sends `Deposit` for the queue's next index: locks `--amount` raw units of the vault's mint and queues a credit to the 20-byte recipient. `--wrap-sol` wraps that many lamports first, in the same transaction, for a vault that holds wrapped SOL. An amount below the minimum, a zero recipient, a missing vault or queue, or a vault of another settlement program is refused by name before any key file is opened. |
| `deposit-queue init --authority-keypair F --settlement P --bridge P --chain-id N [--deadline-secs N] [--max-per-batch N] [--max-per-block N] [--min-amount N] [--fee-lamports N] [--fee-recipient K] [--blocks-per-batch N]` | Creates the chain's deposit queue with the standard parameters unless a flag changes one: a 43200 second deadline, 256 deposits a batch, 4 a block, a minimum of 1000000 raw units and a fee of 100000 lamports paid to the signing key. Refused by name before sending: a value outside its bounds, a per-block limit that times `--blocks-per-batch` is more than the per-batch limit, and a chain whose root has never taken a posted batch. A queue that exists is reported and nothing is sent. |
| `deposit-queue propose ... [--activation-slot N \| --activation-delay-slots N]` | The chain authority proposes new parameters. The default activation is one challenge window plus a small margin from the current slot; a slot outside one to two challenge windows is refused by name. A proposal replaces a pending one, and the output says when it does. |
| `deposit-queue activate --payer-keypair F --settlement P --bridge P --chain-id N` | Makes the pending parameters live once their slot has passed. Anyone can send it. |
| `deposit-queue show --settlement P --bridge P --chain-id N` | Prints `key=value` lines: the live and pending parameters, the activation slot, the queue's count, the cursor's `deposit_next`, the backlog, and the oldest waiting deposit's enqueue time, age and the deadline it is measured against. Sends nothing. |
| `close-deposit --settlement P --bridge P --chain-id N --index N --payer-keypair F` | Closes a deposit record and sends its rent to the depositor, once the batch that credited it is final. The program's refusals are the guard and the command names them. |
| `bridge-config init --authority-keypair F --bridge P --settlement P --inbox P` | Writes the bridge's config once per deployment, signed by the bridge program's upgrade authority. |
| `bridge-config show --bridge P` | Prints the settlement and inbox programs the bridge is bound to. |

Run `rome-zk-ops <command> --help` for every flag. `--rpc-url` defaults to the public Solana devnet endpoint,
`https://api.devnet.solana.com`.

## Registering a verification key

`vkey register` is the one way Rome adds a verification key to a chain's registry. It trusts nothing the operator sends
until it has been checked, in this order, and stops at the first check that fails:

0. `PortalNotCanonical` / `GenesisAllocUnexpected`: before anything is read, the genesis file's alloc must be what the
   rollup template renders: the exit portal (`0x4200…0016`) holds exactly the published runtime
   (`contracts/exit-portal/RomeExitPortal.runtime.hex`) with no storage and no balance, and no other address holds code or
   storage.
1. `ChainIdMismatch`: the chainId in `genesis.json`, the `--chain-id` given and the chain id in the root account on the
   settlement program are one number. A chain that is not registered is refused by name (`ChainNotRegistered`).
2. `GenesisMismatch`: while the root has not moved on from block 0, its block hash and state root are the genesis the chain
   committed to, and they must equal the block the node builds from the operator's `genesis.json` (through the same chain
   spec the node uses). Once the chain has posted a batch that record is gone; the file's sha256 is then compared with the
   one the operator states with `--genesis-sha256` and the one the rebuilt guest embedded, and step 6 anchors the genesis
   another way. Every report prints `genesis_sha256=` so the file Rome checked can be recorded.
3. `ElfMismatch`: the guest is rebuilt from that genesis in the guest-build image of the `--zisk` release
   (`deploy/rollup/guest-build`, the image `./rollup guest-build` runs for that release) at `--guest-tag`, and its sha256
   must be the operator's. Each release has its own image, toolchain and pins (`guest-build/zisk/<release>.env`).
4. `VkeyMismatch`: the programVK is recomputed from the rebuild with the proving keys of that release (`--proving-key-dir`, the
   directory of the `--zisk` release; the image refuses another release's keys) and must
   be the operator's. Each rebuild mounts the keys of its own release: the anchor rebuild of step 6, when it is in
   another release, mounts the directory given with `--anchor-proving-key-dir`. A release with no directory is refused by
   name (`ProvingKeyDirMissing`) before the first rebuild starts. A rebuild that cannot compute it is refused (`ProgramVkNotComputed`), never trusted.
5. The backed balance, while the root is still at genesis. The rebuild reports the balance the genesis gives an account
   as the line the pinned guest build prints (`address=… wei=… lamports=…`, with `remainder_wei=…` when the wei is not a
   whole number of lamports, which is refused as `BalanceNotWholeLamports`). When the lamports are not zero, the command
   reads the chain's `exit_config` for its bridge program (`BridgeNotConfigured` when there is none, `BridgeNotRome` when
   it is not Rome's: `--bridge-program`, default the devnet program in `deploy/rollup/programs.devnet.json`) and the
   chain's vault on that program, and refuses `VaultMissing` (no vault, or no token account), `VaultNotWrappedSol` (the
   vault or its token account holds another mint; the backed balance is in lamports of wrapped SOL),
   `VaultNotOwnedByAuthority` (the token account is not owned by the vault authority) or `VaultUnderfunded` (the token
   balance is below the declared lamports). A genesis with no balance reads no vault. Once the root has moved the balance
   may have been spent, so the vault is not read; step 6 ties the genesis to a key that passed this check.
6. `GenesisUnanchored`: once the root has moved past block 0, `--anchor-guest-tag` is required. It names a guest tag
   that already has a registered key for the chain. The operator's genesis is rebuilt at that tag, and its programVK
   must equal the programVK of a registered entry that is not retired. `--anchor-evm-tag` is the node tag for that
   rebuild (default: `--evm-tag`). `--anchor-zisk` is the release that anchor key was built with (default: `--zisk`); a
   chain moving to a new release anchors on a key of the release it is leaving, and the key must be a live entry under
   that release. The anchor is rebuilt with that release's own proving keys, so an `--anchor-zisk` that names another
   release also needs `--anchor-proving-key-dir`; the directory is refused when the anchor is the same release as `--zisk`. A genesis with a swapped chain config or coinbase would not reproduce it.
   `VkeyUnderOtherZiskVersion` comes before the rebuild: a programVK that a live entry already holds under another
   release is refused, as the program refuses it, because one programVK belongs to one release.
7. `VkeyAlreadyActive` / `VkeyPending`: sending a key that is already registered would move its activation slot, and
   push a working key out of service. An active key is refused. A pending key is refused unless
   `--move-pending-activation` is given.

The activation slot has no default: give `--activation-slot` or `--activation-delay-slots`, not both. The registry
authority's key must be the one in the global config (`NotRegistryAuthority`). Keys are read from the files named and
never printed. The rebuild runs docker (`--docker "sudo docker"` where it needs a prefix) and takes minutes; the image is a
cache hit when `./rollup guest-build` built the same release and tags. `scripts/vkey-register.sh` wraps the command with the flags
checked by name before anything runs, and takes the proving keys from `ZISK_HOME` too. It requires `--zisk` and passes it on, with `--anchor-guest-tag`, `--anchor-zisk`, `--anchor-proving-key-dir`, `--anchor-evm-tag`, `--bridge-program` and `--move-pending-activation`.

## Three rules

- **Dry run unless you say so.** Without `--confirm` a command reads the chain, checks every rule the program
  would check, builds and signs the transaction against an all-zero blockhash, prints it (base64, the decoded
  instruction and its discriminant) and sends nothing. That transaction can never land, because the cluster
  refuses a blockhash it has not seen. `--offline` is a dry run with no RPC at all: the checks that need the
  chain are skipped and the output says so. `--offline` is for commands that send: the read-only commands
  (`chain-id`, `exit-config show`, `vault show`) need an RPC and fail by name without one. `--confirm` and
  `--dry-run` or `--offline` together are refused. A command run with `--confirm` waits until its
  transaction reaches `confirmed`, not `finalized`.
- **V1 transactions only.** Every transaction goes out through `rome-zk-solana-sender`. This crate has no
  other send path and no legacy transaction constructor. Where a command needs a second signer (a reserved
  registration, a proposal paid by a different key), the sender signs for all of them.
- **Keys come from file paths and are never printed.** A key file that cannot be read is refused by name
  (`KeypairUnreadable`) without showing anything about its contents. Only public keys appear in the output.

## Refusals

A command that stops without sending says why by name, as `Name: detail`. Exit code 2 means the request itself
is wrong (a flag combination, a key file, a rule checked before the chain is looked at). Exit code 1 means the
chain or the network said no. A failed read is never taken for a missing account: an unreachable RPC stops
`register` with `NonceLookupFailed`, and does not fall back to nonce 0. In a dry run a failed read skips the
check that needed it and the output says which one.

## The old examples

`register_chain`, `migrate_chain`, `governance` (its three exit-config subcommands) and `init_cursor` are thin
wrappers over this crate, kept so scripts that call them keep working. They take the same flags and, as before,
send unless `--dry-run` is given. The bridge examples `vault` (`init-vault`, `fund`, `show-vault`) and
`release-exit` are wrappers too, and keep their own default: a dry run unless `--confirm` is given. New scripts should call `rome-zk-ops` directly. The registry subcommands of
`governance` (`init-global-config`, `set-registry-entry` and the rest) stay in that example, and send through
the same V1 path.

## In the node image

The root `Dockerfile` builds `rome-zk-ops` next to the sequencer, batcher and derive binaries and installs it at
`/usr/local/bin/rome-zk-ops`, so `docker run --entrypoint rome-zk-ops <image> --help` works on an image built
from this tree. The image runs as uid and gid 999. Published node images include it from `v0.2.1`;
earlier images do not.

## Tests

```
cargo test -p rome-zk-ops
```

The commands are written against a small `Chain` trait (read an account, read the slot, read an EVM genesis,
send one instruction). The tests run every command over a fake that records what it was asked to send, so they
check that a dry run sends nothing, that a confirmed run sends exactly one transaction with the right signers,
and that each refusal fires by name.
