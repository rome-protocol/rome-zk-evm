# zk-bridge

THE VAULT — the minimal SPL-token escrow program that releases a proved settlement exit to its rightful
recipient. This is the zk lane's first program to CPI the real SPL Token program, and the first program in
this workspace that is deployed for real (as opposed to `programs/zk-settlement/tests/fixtures/stub-bridge`,
a test-only stand-in that is never shipped).

Custody lives here. Verification (`ProveExit`/`ConsumeExit`) lives in `programs/zk-settlement`, which this
program never modifies — it only CPIs `ConsumeExit`, signed by its own `["exit_consumer", chain_id]` PDA. A
keypair can never sign as a PDA, so once a chain's `exit_config.bridge_program` names this program's id
(a governance action outside this program's own scope), that PDA is the *only* address that can ever
authorise a release for that chain — see `rome_zk_layouts::exit::exit_consumer_pda`'s own doc for why.

## Instructions

- **`InitVault { chain_id, mint, mint_decimals, settlement_program }`** — creates the vault's own SPL Token
  account (`["vault", settlement_program, chain_id, mint]`, owned by the SPL Token program) and a small
  config record (`["vault_config", settlement_program, chain_id]`, owned by this program) naming which
  settlement program's `exit_record`s this vault releases against, which mint it holds, and that mint's
  decimals. Refuses a second call for the same `(settlement_program, chain_id)` (`VaultAlreadyInitialized`).
  **Gated by the settlement chain authority:** the caller passes a `chain_authority` signer plus the
  settlement program's own `["root", chain_id]` account (read-only); the instruction requires that `root`
  account be owned by `args.settlement_program` and live at exactly that PDA, decodes it, and requires
  `chain_authority` to sign AND its key to equal the decoded `root.authority` (`NotChainAuthority`
  otherwise) — the identical gate `zk-settlement::governance::require_chain_authority` applies to
  `ProposeExitConfig`. `vault_config.authority` records that chain authority.

  **What this gate guarantees, precisely:** a `vault_config` naming a given `settlement_program` can only
  ever be created by that settlement's own real `root.authority` — an attacker can never forge
  `{settlement_program = X, authority = them}` for an `X` whose root they do not control. It does **not**,
  by itself, stop an attacker from creating *some* config naming their *own* hostile `settlement_program` —
  they trivially control that program's root. The property that actually closes the front-run is the vault
  PDAs' **keying**, below.

  **The address is un-frontrunnable:** `vault_config`/`vault`/`vault_authority` are keyed by
  `[settlement_program, chain_id]` (`vault` also by `mint`), not `chain_id` alone. So a hostile
  `settlement_program` argument does not let an attacker occupy the real chain's vault slot at all — it
  only ever creates a config at `pda(hostile_settlement, chain_id)`, a **different address** than
  `pda(real_settlement, chain_id)`. Occupying the real address requires signing as `real_settlement`'s
  own `root.authority`, which only the real chain authority can do. A front-runner can neither block nor
  redirect the real authority's `InitVault`: whichever order the two calls land in, each creates its own
  config at its own address, and only `pda(real_settlement, chain_id)` is ever the one governance later
  wires into `exit_config.bridge_program` — a hostile config at a different address is simply never
  funded or wired, and that wiring additionally verifies `vault_config.settlement_program`/`authority`
  before funding/wiring as defense in depth (belt and suspenders — the CORE property is the keying, not
  this check).
  `mint_decimals` is also checked against the mint account's own real decimals byte (offset 44 of the
  standard 82-byte layout) — `MintDecimalsMismatch` on a mismatch, so an operator typo can never mis-scale
  every future `ReleaseExit` payout.
- **`Fund { chain_id, amount }`** — a permissionless SPL transfer into the vault. Anyone may top it up;
  nothing about who funds it is trusted, only where the tokens land (the vault's own PDA-derived account).
- **`ReleaseExit { chain_id, message_hash }`** — the core instruction. See "Fund-safety invariants" below.
- **`InitBridgeConfig { settlement_program, inbox_program }`** — writes the bridge's one config account
  (`["bridge_config"]`) once. The signer must be this program's own upgrade authority: the instruction reads
  the program's real `ProgramData` account and refuses a wrong address, a wrong owner, an immutable program or
  a signer that is not the stored authority. It also refuses a zero program id and a second write. A
  pre-funded config address is adopted, not refused.
- **`InitDepositQueue { chain_id, settlement_program, params }`** — creates a chain's deposit queue at
  `["deposit_queue", settlement_program, chain_id]`, empty and with its hash chain at the seed value. The chain
  authority signs, proved the way `InitVault` proves it: `root` must be the config's settlement program's
  `["root", chain_id]` account, owned by that program, and `root.authority` must sign. It refuses a settlement
  program other than the config's, a reserved chain id, a registry that is not at the config's settlement
  program's registry address, not owned by it, for another chain, or naming an inbox other than the config's, a
  vault whose mint has more than 9 decimals, and any parameter outside the bounds below. A pre-funded queue
  address is adopted, and a queue that already holds data is refused.
- **`ProposeDepositParams { chain_id, activation_slot, params }`** and **`ActivateDepositParams { chain_id }`**
  — the chain authority proposes new parameters (same root check, same bounds), with an activation slot at least
  one `root.challenge_window_slots` away; anyone may activate them once that slot is reached. A proposal is
  refused while another is pending.

- **`Deposit { chain_id, amount, l2_recipient }`** (tag 3) — a user locks `amount` base units of the vault's mint
  in the vault and queues a credit of the same value, in gwei, to a 20-byte address on the chain. The depositor
  signs and pays the record's rent and the queue's fee (to the fee recipient in the queue's active parameters);
  the record's sender is the depositor's wallet. The record is written at `["deposit_record", settlement_program,
  chain_id, index]` for the queue's current `count`, the queue's `count` and hash chain advance, and a record
  address that was pre-funded is adopted. Every account is bound by address: the vault config and the queue sit at
  their PDAs under the vault config's settlement program, and the chain's root, registry and exit config are that
  program's accounts. Refused by name: a vault config or queue of another settlement program or chain; a chain whose
  root or registry is gone or not owned by the settlement program (a reclaimed chain keeps its queue and vault, so
  a deposit there could never be credited or refunded); an exit config naming another bridge; a zero recipient or
  the exit portal; a fee recipient other than the parameter; an amount below the minimum; a mint whose amount does
  not fit in gwei; a record address that already holds a record.
- **`CloseDeposit { chain_id, index }`** (tag 8) — permissionless; refunds a record's rent to its sender once a
  finalized batch has credited the deposit and that batch is final. The cursor must be the chain's, at the inbox's
  address, owned by the inbox and at version 2: `deposit_next > index` means a batch has credited it, `index <
  deposit_final` means that batch is final. The rent always goes to the record's sender. The vault and the fee
  recipient are not touched.

**Parameter bounds, fixed in the program (an upgrade changes them):** an inclusion deadline of 1 to 24 hours;
`1 <= max_per_block <= max_per_batch <= 256`; `min_amount >= 1` base unit; a fee of at most 0.01 SOL; and a fee
recipient account that holds the rent-exempt minimum for an empty account (checked again on activation) and is
neither executable nor a sysvar.

## Fund-safety invariants

**The recipient is never an instruction argument.** `ReleaseExit` takes a caller-supplied `recipient_ata`
account, but never trusts it: it reads `record.sol_recipient` off the settlement program's own
`exit_record`, derives that recipient's Associated Token Account for the vault's mint itself, and refuses
(`WrongRecipientAta`) before any CPI or transfer runs if the supplied account is anything else. There is no
instruction argument anywhere in this program that names a payment destination — the wire format simply
does not have one.

**Consume-before-pay, atomically.** The settlement program's `ConsumeExit` is CPI'd — closing the exit
record and refunding its rent to `record.payer` — *before* the SPL transfer, in the same transaction. A
failed CPI aborts the whole transaction (Solana's own atomicity), so there is no reachable state where a
transfer happens without the record having just been consumed. Because `ConsumeExit` recycles the record
(reassigns it to the system program), a second `ReleaseExit` against the same `message_hash` fails the very
first seeds/owner check (`WrongSettlementOwner`) — the record simply no longer exists at the settlement
program. No double payout is possible.

**Decimal scaling rounds down.** `exit_record.amount` is u128 wei of an 18-decimal EVM asset;
`mint_amount = amount / 10^(18 - mint_decimals)`. Integer division truncates: any sub-unit remainder (dust)
is never transferred and simply stays in the vault's own SPL balance. Refuses `mint_decimals > 18`
(`InvalidMintDecimals`) — the exponent would go negative.

**Native asset only (v1).** `record.asset != [0; 20]` is refused (`UnsupportedAsset`) — the same restriction
`zk-settlement`'s own `ProveExit` already enforces upstream; a per-asset vault is a later seam.

**The exit record must belong to the registered settlement program.** `exit_record`'s owner is checked
against `vault_config.settlement_program` before its data is ever decoded (`WrongSettlementOwner`) — a
forged or misdirected record cannot be substituted.

**The refund destination is the record's own payer.** `ReleaseExit` checks `payer_refund == record.payer`
itself (`WrongPayerRefund`) before the settlement CPI ever runs — defense in depth: `zk-settlement`'s own
`ConsumeExit` enforces the identical property on the CPI it is about to receive (`InvalidArgument`), so
funds are safe either way, but this program's own named error is what a caller sees when it names the wrong
account, and `release_to_wrong_payer_refund_is_refused` (real BPF) plus a mutation of this program's own
check (which turns that named error into the CPI's generic `InvalidArgument`) prove the guard is not
decorative.

## No `spl-token`/`spl-token-interface` Cargo dependency — by design

Before the crate bump this workspace pinned `solana-program = "=2.1.6"`, and the
`spl-token`/`spl-token-interface`/`spl-associated-token-account-interface` releases of that day needed a
newer `solana-pubkey`/`solana-instruction`. A newer line was already in the lockfile through an unrelated
dependency, so two different `Pubkey`/`Instruction` types sat side by side and passing ours into the SPL
crates did not compile.
That is no longer the case. On `solana-program = "=4.1.0"` the current releases (`spl-token` 9.0.0,
`spl-token-interface` 3.0.0, `spl-associated-token-account` 8.0.0, `spl-associated-token-account-interface`
2.0.0) resolve onto that one `solana-program` and accept its `Pubkey` and `Instruction` directly. This was
checked with a throwaway crate seeded from the workspace `Cargo.lock`: `cargo tree -i
solana-program` shows a single node, and `transfer`, `get_associated_token_address` and
`create_associated_token_account_idempotent` compile and run against our types. Whether to swap the
hand-rolled wire below for those crates is a separate decision, and the bump did not take it.

`src/token.rs` hand-rolls the small, stable pieces of the SPL Token / Associated-Token-Account wire format
this program needs instead — the same kind of fixed-layout encoding `rome_zk_layouts` already hand-rolls
for every other account this workspace reads, applied here to a program this workspace does not own:
`TokenInstruction::Transfer`/`InitializeAccount3` (both unchanged since the SPL Token program's original
release), the 165-byte token account layout (only the `amount` field is ever read back), and the
Associated-Token-Account seed formula. This program never executes SPL Token logic itself — every state
change to a token account happens via a real CPI into the real, deployed program; only the *encoding* of
those CPIs is hand-rolled.

## Building and testing the real SPL Token program for tests

`solana-program-test` does not bundle the SPL Token program — it must be loaded explicitly, from a real
`.so`. Because `spl-token` is not a dependency of this workspace (see above), that `.so` is built from a
throwaway, standalone manifest (`spl-token = "=9.0.0"`, generated fresh in a temp directory, never
committed) whose own `cargo metadata` resolves and fetches the crate independently of this workspace's own
`Cargo.lock`:

```sh
SPL_TMPDIR=$(mktemp -d)
mkdir -p "$SPL_TMPDIR/src" && echo '' > "$SPL_TMPDIR/src/lib.rs"
printf '[package]\nname = "spl-token-fetch"\nversion = "0.0.0"\nedition = "2021"\npublish = false\n[dependencies]\nspl-token = "=9.0.0"\n' > "$SPL_TMPDIR/Cargo.toml"
SPL_TOKEN_MANIFEST=$(cd "$SPL_TMPDIR" && cargo metadata --format-version=1 | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(p["manifest_path"] for p in d["packages"] if p["name"]=="spl-token" and p["version"]=="9.0.0"))')
cargo build-sbf --manifest-path "$SPL_TOKEN_MANIFEST" --sbf-out-dir target/deploy
rm -rf "$SPL_TMPDIR"
```

This produces a genuine `spl_token.so` — `spl-token = "=9.0.0"`'s own `[package.metadata.solana]
program-id` is `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` (the well-known mainnet address) and its `[lib]
crate-type = ["cdylib", "lib"]` builds under plain `cargo build-sbf` with no wrapper needed. Run the
block above before `cargo test`; the build lands in `target/deploy`, where `tests/common` loads it.

### The real Associated Token Account program too

`release_exit.rs`'s `create_ata_idempotent_then_release_lands_for_a_recipient_with_no_ata` CPIs the real
Associated Token Program (`create_recipient_ata_idempotent_ix`, `zk-bridge-client`), so `tests/common`'s
`base_program_test` loads a genuine `.so` for it. Unlike `spl_token.so` above, a throwaway-manifest
`cargo build-sbf` does NOT work here: `spl-associated-token-account = "=4.0.0"`'s own `spl-token-2022`
dependency floats to a release whose `solana-zk-token-sdk` fails to compile on a fresh, uncommitted-
lockfile resolve. Instead, run a read-only `solana program dump` of the real, currently-deployed devnet
program into `target/deploy` — no build, no dependency resolution at all — and check the dumped `.so`
against a pinned sha256 before use (CI carries the same pin). Do it right after the `spl_token.so` build,
before `cargo test`. The program is loaded at the real, well-known
`ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` address (`zk_bridge::token::ASSOCIATED_TOKEN_PROGRAM_ID`).

```sh
solana program dump ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL target/deploy/spl_associated_token_account.so \
  --url https://api.devnet.solana.com
echo "6804554e69fd3a58caa191dc4a58f4c67223d30ca28ab8987f39fc18d2f7374d  target/deploy/spl_associated_token_account.so" \
  | sha256sum --check
```

Full local test run:

```sh
cargo build-sbf --manifest-path programs/zk-bridge/Cargo.toml --sbf-out-dir <workspace>/target/deploy
cargo build-sbf --manifest-path programs/zk-settlement/Cargo.toml --sbf-out-dir <workspace>/target/deploy
# ... the spl-token build above ...
# ... the Associated Token Account dump above ...
cargo test -p zk-bridge -p zk-bridge-client
```

## Measured compute units (real BPF, `programs/zk-bridge/tests/{vault,release_exit}.rs`)

`InitVault`'s mint and `ReleaseExit`'s mint/recipient are pinned to `rome_zk_testkit::fixed_mint_pubkey`/
`fixed_recipient_pubkey` (not `Pubkey::new_unique()`) — `vault_token_pda`/`recipient_ata` are both PDAs
seeded by these values, so a random mint/recipient previously made every figure below vary run to run with
that PDA's own bump-seed search depth. Pinned, every figure below is bit-exact across
repeated runs of the same binary (verified: 3 consecutive `cargo test` runs, identical CU each time).

- `ReleaseExit`, whole transaction (the `ConsumeExit` CPI + the SPL transfer included): **45,873 CU**
  (54,506 before the crate bump), comfortably inside the 1.4M per-transaction ceiling.
- `ConsumeExit`, the nested CPI frame within that same transaction (real bridge, not a test-only stub):
  **15,835 CU** (15,794 before the crate bump) — unaffected by the vault re-key (it derives `zk-settlement`'s own PDA, not
  `zk-bridge`'s).
- `InitVault` (incl. the `InitializeAccount3` CPI, the chain-authority-gate's root read, and the
  mint_decimals check): **30,753 CU** (39,998 before the crate bump). `Fund` (incl. the SPL `Transfer`
  CPI): **8,902 CU** (7,354 before the crate bump, measured against a random mint, so not comparable).

Deposit setup (same tests, `programs/zk-bridge/tests/deposit_setup.rs`; printed by each test):
`InitBridgeConfig` **12,718 CU** (4,948 to 5,014 for the refusals, 14,271 when the address was pre-funded and
adopted), `InitDepositQueue` **19,881 CU** (it computes the queue's seed hash with the keccak syscall),
`ProposeDepositParams` **12,906 CU**, `ActivateDepositParams` **6,686 CU**.

Deposits (`programs/zk-bridge/tests/deposit.rs`; printed by each test, against fixture accounts for the settlement
side): `Deposit` **36,039 CU** for the first deposit of a queue (33,040 and 34,540 for the next two of the golden
test), **39,353 CU** when the record address was pre-funded
and adopted, **54,751 CU** for the wrap-SOL instructions and a `Deposit` in one transaction; a refusal costs
5,767 to 26,626 CU. `CloseDeposit` **12,453 CU** (5,135 to 11,717 CU for the refusals).

(Re-key note: promoting `settlement_program` into the vault PDA seeds shifts every PDA's own
bump-seed search depth, so these figures moved from their earlier values — down, in this measurement,
though the direction is not guaranteed in general — without any change in what each instruction does; still
bit-exact across repeated runs on the same binary.)

## PDAs

| PDA | Seeds | Owner |
|---|---|---|
| Vault config | `["vault_config", settlement_program, chain_id]` | this program |
| Vault token account | `["vault", settlement_program, chain_id, mint]` | the SPL Token program |
| Vault authority | `["vault_authority", settlement_program, chain_id]` | none (never holds data; a pure signer identity) |
| Bridge config | `["bridge_config"]` | this program |
| Deposit queue | `["deposit_queue", settlement_program, chain_id]` | this program |
| Exit consumer | `["exit_consumer", chain_id]` | none (the identity this program CPI-signs `ConsumeExit` as) |

Keying the first three by `settlement_program` is what makes the real chain authority's vault address
un-frontrunnable — see `InitVault`'s own bullet above. `exit_consumer` is unaffected: it identifies THIS
program to `zk-settlement`, not a settlement program to this one.

## What this program does not do

No deposit path yet (the queue and its parameters exist, the deposit instruction does not) — `InitVault`/`Fund` stand in for a real bridge-in until a later step (the first live
release is from an operator-funded devnet SPL vault; deposits come later). No multi-asset vault (one
mint per `(settlement_program, chain_id)` in v1; the `["vault", settlement_program, chain_id, mint]`
seed already leaves room for more). No recipient-ATA auto-creation — `ReleaseExit` requires the
recipient's ATA to already exist; creating it on the fly would need a CPI into the
Associated-Token-Account program, which this program does not add (the address formula alone,
hand-rolled in `src/token.rs`, is all `ReleaseExit` needs to *check* the caller-supplied account
against). Clients create the recipient's ATA themselves, in the same transaction if they like
(`create_recipient_ata_idempotent_ix` in `zk-bridge-client`; see the test section above).
