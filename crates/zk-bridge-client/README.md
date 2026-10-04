# zk-bridge-client

A thin, async-runtime-agnostic client for [`programs/zk-bridge`](../../programs/zk-bridge): instruction
builders, PDA derivation, and account decoding for the vault config account. Like
[`zk-settlement-client`](../zk-settlement-client), it has no async runtime opinions: this crate only builds
`Instruction`s and decodes bytes, and sending transactions is the caller's job. The operator commands that
send them are in [`rome-zk-ops`](../rome-zk-ops).

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
  what `rome-zk-ops release-exit` prepends to every `ReleaseExit` it sends, so a first-time recipient's
  missing ATA never blocks a release.
- `decode_vault_config_account` — decodes a `vault_config` account's bytes into `chain_id`,
  `settlement_program`, `mint`, `mint_decimals`, `authority`.
- `check_vault_settlement(cfg, expected_settlement)` — `vault_config`'s address is a PDA *derived from* a
  caller-supplied `settlement_program`, so nothing on the wire stops a stale/mistyped/wrong-network value
  from resolving to some account this program owns whose *recorded* `settlement_program` disagrees with
  what was intended. Refuses `VaultSettlementMismatch` when the decoded account's own field does not
  match; `rome-zk-ops vault fund` runs this before building `fund_ix`, let alone sending it.
- `vault_tool` — the planning logic behind `rome-zk-ops vault`, kept here so its tests run over fakes:
  `plan_init_vault` (an existing vault is reported and never created again), `plan_fund` (refuses
  `VaultSettlementMismatch` before any instruction is built), `describe_vault` for `vault show`, and `execute`,
  which sends exactly once with `--confirm` and never in a dry run.

## Operator commands: `rome-zk-ops vault` and `rome-zk-ops release-exit`

Use [`rome-zk-ops`](../rome-zk-ops): `vault init`, `vault fund` and `vault show` create, fund and show a
chain's vault, and `release-exit` releases a proved exit to its recipient. Each is a dry run unless
`--confirm` is given, reads keys from file paths and never prints them, and sends one V1 transaction
through `rome-zk-solana-sender`. Their flags and refusals are in that crate's README.

```sh
# Creates the vault. The authority is both the payer and the chain authority, and must equal the
# settlement root's authority for the chain. A vault that already exists is reported and nothing is sent.
rome-zk-ops vault init --authority-keypair /path/to/chain_authority.json \
  --mint <mint pubkey> --mint-decimals 6 \
  --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> [--confirm]

# Anyone can fund. Reads vault_config back and derives the mint and the funder's token account from it.
# Refused by name (VaultSettlementMismatch) before fund_ix is built if the vault does not belong to --settlement.
rome-zk-ops vault fund --payer-keypair /path/to/funder.json --amount <u64, raw units> \
  --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> --chain-id <u64> [--confirm]

# Read-only: decodes vault_config and prints the vault's token balance.
rome-zk-ops vault show --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
  --chain-id <u64>

# Reads the exit_record for the message hash (refused by name if it does not exist or is not PROVED) and the
# vault, derives the recipient's token account, and sends [create_recipient_ata_idempotent_ix, release_exit_ix].
rome-zk-ops release-exit --settlement <settlement program pubkey> --bridge <zk-bridge program pubkey> \
  --chain-id <u64> --message-hash 0x<32 bytes> --payer-keypair /path/to/payer.json [--confirm]
```

Add `--rpc-url URL` to each command to choose the Solana endpoint.

The `vault` example (`init-vault`, `fund`, `show-vault`) and the `release-exit` example are thin wrappers over
these commands, kept so scripts that call them keep working. They take the same flags and keep the same
default, a dry run unless `--confirm` is given:

```sh
cargo run -p zk-bridge-client --example vault --features devnet-driver -- init-vault ...
cargo run -p zk-bridge-client --example release-exit --features devnet-driver -- --settlement ... --confirm
```

The `devnet-driver` feature is empty now. It stays so commands that pass it keep working; the library pulls in
no RPC client or async runtime either way.

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
