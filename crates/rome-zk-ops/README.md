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

Run `rome-zk-ops <command> --help` for every flag. `--rpc-url` defaults to the public Solana devnet endpoint,
`https://api.devnet.solana.com`.

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
