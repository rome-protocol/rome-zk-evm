# zk-bridge-client

A thin, async-runtime-agnostic client for [`programs/zk-bridge`](../../programs/zk-bridge): instruction
builders, PDA derivation, and account decoding for the vault config account. Mirrors
[`zk-settlement-client`](../zk-settlement-client)'s own shape and reasoning — no async runtime opinions;
this crate only builds `Instruction`s and decodes bytes, sending transactions is the caller's job.

## What it provides

- `init_vault_ix`, `fund_ix`, `release_exit_ix` — the three instruction builders, each with the exact
  account order and metas the program expects (see each function's own doc, and
  `programs/zk-bridge/src/instruction.rs`'s own doc for the canonical list). `init_vault_ix` takes a
  `chain_authority` pubkey (must sign the transaction) and derives the settlement `["root", chain_id]`
  account internally from `settlement_program` — `InitVault` is gated by the settlement chain authority
  (see `programs/zk-bridge/README.md`).
- `vault_config_pda`, `vault_token_pda`, `vault_authority_pda` — re-exported from `zk_bridge::state`, the
  program's own single definition of every seed formula.
- `recipient_ata(recipient, mint)` — the exact Associated Token Account address `ReleaseExit` itself derives
  and checks the caller-supplied `recipient_ata` account against (`zk_bridge::token::
  get_associated_token_address`, hand-rolled — see `programs/zk-bridge/README.md`'s "No spl-token Cargo
  dependency" section for why).
- `create_recipient_ata_idempotent_ix(payer, recipient, mint)` — Associated Token Program
  `CreateIdempotent`: creates the recipient's ATA if it does not exist yet, a documented no-op if it does.
  `programs/zk-bridge/src/release.rs` only derives and checks `recipient_ata`, never creates it — this is
  what the `release-exit` example (below) prepends to every `ReleaseExit` it sends, so a first-time
  recipient's missing ATA never blocks a release.
- `decode_vault_config_account` — decodes a `vault_config` account's bytes into `chain_id`,
  `settlement_program`, `mint`, `mint_decimals`, `authority`.
- `check_vault_settlement(cfg, expected_settlement)` — `vault_config`'s address is a PDA *derived from* a
  caller-supplied `settlement_program`, so nothing on the wire stops a stale/mistyped/wrong-network value
  from resolving to some account this program owns whose *recorded* `settlement_program` disagrees with
  what was intended. Refuses `VaultSettlementMismatch` when the decoded account's own field does not
  match; the `vault` example's `fund` subcommand runs this before building `fund_ix`, let alone sending it.

## The `vault` example (operator tool)

`init-vault`, `fund`, `show-vault` for `programs/zk-bridge`'s vault — same shape as `release-exit`
(subcommands mirror `zk-settlement-client/examples/governance.rs`'s own shape): keys always read from FILE
PATHS and never printed, **`--dry-run` is the DEFAULT** wherever a send is possible, `--confirm` opts in.
Every account list comes straight from `zk_bridge_client::{init_vault_ix, fund_ix}`.

```sh
# Idempotent — a second run against an already-initialized vault prints its decoded vault_config and
# does nothing else. authority is BOTH the payer and the chain authority (must equal the settlement
# root.authority for chain-id).
cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
  init-vault --authority-keypair /path/to/chain_authority.json \
  --mint <mint pubkey> --mint-decimals 6 \
  --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> \
  [--rpc-url URL] [--confirm]

# Permissionless. Reads vault_config back on-chain and derives the mint/funder ATA from it — never a
# re-typed --mint. Refuses by name (VaultSettlementMismatch) before building fund_ix if the vault this
# address resolves to does not actually belong to --settlement.
cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
  fund --payer-keypair /path/to/funder.json --amount <u64, raw units> \
  --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> \
  [--rpc-url URL] [--confirm]

# Read-only, no signer: decodes vault_config and prints the vault's own SPL token balance.
cargo run -p zk-bridge-client --example vault --features devnet-driver -- \
  show-vault --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
  --chain-id <u64> [--rpc-url URL]
```

Every chain read happens before any keypair is touched and before branching on `--dry-run`/`--confirm` —
an unreachable RPC, a not-yet-created vault (`fund`), or a settlement mismatch (`fund`) all fail loud, by
name, in EITHER mode, the same "reads before keys, before branches" shape `release-exit.rs` already uses.

## The `release-exit` example (operator tool)

```
cargo run -p zk-bridge-client --example release-exit --features devnet-driver -- \
  --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
  --chain-id <u64> --message-hash 0x<32 bytes> --payer-keypair /path/to/payer.json \
  [--rpc-url URL] [--confirm]
```

Reads the settlement `exit_record` for `message_hash` (refuses by name if it does not exist, or exists
but is not `PROVED`) and the vault's `vault_config` (for its mint), derives the recipient's ATA, and
builds `[create_recipient_ata_idempotent_ix, release_exit_ix]` as one transaction — printing every
account before doing anything else. **`--dry-run` is the DEFAULT**: both instructions are signed against
an all-zero placeholder blockhash and printed as base64, nothing is sent to any cluster (mirrors
`zk-settlement-client/examples/governance.rs`'s own `--dry-run` shape). Only `--confirm` sends for real.
Both chain reads happen before the payer keypair is ever touched and before either branch, so an
unreachable RPC or a not-yet-proved record fails loud, by name, in EITHER mode — this tool never silently
proceeds toward a send it cannot justify.

## Building a real `ReleaseExit` transaction

`release_exit_ix`'s `mint`, `settlement_program`, and `recipient` parameters are read off already-existing
on-chain state — `mint`/`settlement_program` from the vault's own `vault_config` account (via
`decode_vault_config_account`), `recipient` from the settlement program's `exit_record` account (via
`zk_settlement_client::decode_exit_record_account`'s own `sol_recipient` field). The program independently
re-derives and checks every one of them; passing anything else here only produces a transaction the program
refuses, never one that moves funds anywhere but `record.sol_recipient`'s own ATA.

## No `spl-token`/`spl-associated-token-account-interface` Cargo dependency

Same reasoning as `programs/zk-bridge` — see that program's own README for the history.
This crate re-uses `zk_bridge::token`'s hand-rolled address formula rather than
depending on either crate itself.
