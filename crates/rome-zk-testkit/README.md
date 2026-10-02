# rome-zk-testkit

Shared `solana-program-test` fixtures for every program's and crate's integration test suite. Before
this crate existed, the same handful of helpers — building a `ProgramTest` that loads the real,
`cargo build-sbf`-compiled program binary; computing a rent-exempt lamport amount; building a
`batch_cursor` or root account's bytes by hand; a funded keypair; the attacker-prefunds-a-PDA griefing
primitive; sending a transaction and reading back its consumed compute units; a fixed program id for a
CU-gate test to load a program under — were pasted, byte-for-byte or near enough, into more than a dozen
separate test files across `programs/zk-inbox`, `programs/zk-settlement`, `rome-zk-derive` and
`rome-zk-batcher`. This crate is the one copy every test file now imports instead.

## What it provides

- `sbf_out_dir()` — the directory `cargo build-sbf` writes `.so` files to, resolved the same way from
  any workspace member.
- `program_test(programs, bump_cu)` — builds a `ProgramTest` that loads one or more real `.so` programs
  (plain or upgradeable loader), asserting each binary exists first and naming the `cargo build-sbf`
  command that produces it. `bump_cu` raises the compute budget to 1,400,000 for ordinary tests; a test
  that measures CU against its own `ComputeBudgetInstruction::set_compute_unit_limit` passes `false` so
  the harness never masks the exact limit under test.
- `rent_exempt(space)`, `funded_keypair()` — the small primitives every fixture above is built from.
- `cursor_account(...)`, `root_account_with_authority(...)` — hand-built account bytes for the inbox
  `batch_cursor` and the settlement root account, via `rome_zk_layouts`'s own layout constants (never a
  second copy of an offset).
- `prefund_pda(ctx, pda)` — sends a small transfer to `pda` from a fresh, unrelated keypair before the
  real creator's transaction lands: the griefing primitive every `*_succeeds_even_when_an_attacker_
  prefunds_*` test reproduces.
- `send_measuring_cu(ctx, ixs, payer, extra_signers)` — sends one transaction and returns the real,
  BPF-measured compute units it consumed (`process_transaction_with_metadata`, never replay/estimation,
  which inflates CU 3-4x) alongside its raw log messages. The one send-and-observe helper every CU-pinning
  test in this workspace uses; a caller that only wants the plain success/failure result, or only the CU
  figure, adapts the return value at its own call site.
- `fixed_inbox_program_id()`, `fixed_settlement_program_id()`, `fixed_stub_bridge_program_id()`,
  `fixed_zk_bridge_program_id()` — fixed, distinct program ids for a CU-gate test to load the
  inbox/settlement/stub-bridge/zk-bridge program under, instead of `Pubkey::new_unique()`. A PDA's
  bump-seed search depth (and therefore the measured CU of any instruction that derives one) depends on the
  program id the search runs against, so a random id would make the CU figures these tests assert
  run-to-run-incomparable.
- `fixed_mint_pubkey()`, `fixed_recipient_pubkey()` — same rationale, for a test's own mint/
  recipient pubkeys where a program's PDA is seeded (in part) by them (`zk-bridge`'s `vault_token`/
  `recipient_ata`) rather than by the program id.

## What it guarantees

- **One definition, every call site.** Every helper above has exactly one implementation; every
  program's and crate's own test files import it rather than redefining it. A fixture bug is now fixed
  in one place, not chased across a dozen files.
- **Dev-dependency only, always.** No program or shipped binary depends on this crate — it exists to
  build test fixtures, nothing a running program or service needs. Every consumer picks it up under its
  own `[dev-dependencies]`.

## Config

None — this is a pure test-fixture library crate with no runtime configuration.

## How to test

```sh
cargo test -p rome-zk-testkit
```

Every consumer's own test suite (`cargo test -p zk-inbox`, `-p zk-settlement`, `-p rome-zk-derive`,
`-p rome-zk-batcher`) exercises these fixtures indirectly, against the real `cargo build-sbf`-compiled
programs.

## Design references

The lane design covers the inbox batch/cursor accounts these fixtures build and the settlement root account.
This crate is the one owner of the program-test fixtures in the workspace.

## Depends on

`rome-zk-layouts` for the account layout constants `cursor_account`/`root_account_with_authority` write;
`solana-program`, `solana-program-test`, `solana-sdk` (all pinned) for the `ProgramTest`/`Account`/
`Keypair` types these fixtures build.
