//! Shared `solana-program-test` fixtures for every program's and crate's integration test suite. Before this crate,
//! `program_test`/`rent_exempt`/`sbf_out_dir`/`cursor_account`/`root_account_with_authority`/
//! `funded_keypair`/`prefund_pda` were each pasted, byte-for-byte or near enough, into 11 separate test files
//! across `programs/zk-inbox`, `programs/zk-settlement`, `rome-zk-derive` and `rome-zk-batcher` — this crate is the
//! one copy every test file now imports instead.
//!
//! **Dev-dependency only.** Never add this crate to a program's or a shipped binary's own
//! `[dependencies]` — it exists to build a `ProgramTest` and hand-built account fixtures, nothing a
//! running program or service needs.

use solana_program::{instruction::Instruction, pubkey::Pubkey, rent::Rent};
// `system_program`/`system_instruction` moved out of `solana_program`'s root re-export in the Agave 4.x line (API
// fallout).
use solana_program_test::{BanksClient, ProgramTest, ProgramTestContext};
use solana_sdk::{
    account::{Account, AccountSharedData},
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use solana_system_interface::program as system_program;

/// The directory `cargo build-sbf` writes `.so` files to, from any workspace member's
/// `CARGO_MANIFEST_DIR` (`programs/<p>/../../target/deploy` and `crates/<c>/../../target/deploy` are the
/// same directory: the workspace root's `target/deploy`).
pub fn sbf_out_dir() -> String {
    format!("{}/../../target/deploy", env!("CARGO_MANIFEST_DIR"))
}

/// One program this test run loads into `ProgramTest` — either via the plain `bpf_loader`
/// (`upgradeable: false`, `ProgramTest::add_program`) or the upgradeable loader (`upgradeable: true`,
/// `ProgramTest::add_upgradeable_program_to_genesis` — required by any instruction that checks the
/// program's real upgrade authority, e.g. zk-settlement's `InitGlobalConfig`).
pub struct ProgramSpec {
    pub name: &'static str,
    pub program_id: Pubkey,
    pub upgradeable: bool,
}

impl ProgramSpec {
    pub fn new(name: &'static str, program_id: Pubkey) -> Self {
        Self {
            name,
            program_id,
            upgradeable: false,
        }
    }

    pub fn upgradeable(name: &'static str, program_id: Pubkey) -> Self {
        Self {
            name,
            program_id,
            upgradeable: true,
        }
    }
}

/// Builds a `ProgramTest` loading the real, `cargo build-sbf`-compiled `.so` for every program in
/// `programs` (asserting each one exists first, naming the `cargo build-sbf` command that produces it),
/// with `prefer_bpf(true)` so `solana-program-test` runs the real BPF bytecode rather than a host-native
/// shortcut (real CU figures, real account-size limits). `bump_cu` sets the compute budget to 1,400,000
/// (comfortably above any single instruction this workspace's tests exercise) — pass `false` for a test
/// that measures CU against its *own* `ComputeBudgetInstruction::set_compute_unit_limit`, where the
/// harness bumping the ceiling first would defeat the point of the test.
pub fn program_test(programs: &[ProgramSpec], bump_cu: bool) -> ProgramTest {
    let dir = sbf_out_dir();
    let mut pt = ProgramTest::default();
    pt.prefer_bpf(true);
    if bump_cu {
        pt.set_compute_max_units(1_400_000);
    }
    for p in programs {
        let so_path = std::path::Path::new(&dir).join(format!("{}.so", p.name));
        assert!(
            so_path.exists(),
            "{}.so not found at {dir} — run `cargo build-sbf --manifest-path programs/{}/Cargo.toml` first",
            p.name,
            p.name.replace('_', "-"),
        );
        std::env::set_var("SBF_OUT_DIR", &dir);
        if p.upgradeable {
            pt.add_upgradeable_program_to_genesis(p.name, &p.program_id);
        } else {
            pt.add_program(p.name, p.program_id, None);
        }
    }
    pt
}

/// The rent-exempt minimum for an account of `space` bytes, under the default (test) rent schedule.
pub fn rent_exempt(space: usize) -> u64 {
    Rent::default().minimum_balance(space)
}

/// A fresh, unfunded keypair — the caller funds it (or not) via `ProgramTest::add_account`/
/// `ProgramTestContext::set_account` as the test needs.
pub fn funded_keypair() -> Keypair {
    Keypair::new()
}

/// An inbox `batch_cursor` account at `next_batch`, owned by `program_id` — the layout `rome_zk_layouts::cursor`
/// defines.
pub fn cursor_account(program_id: Pubkey, chain_id: u64, next_batch: u64) -> Account {
    let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
    d[rome_zk_layouts::cursor::OFF_MAGIC..rome_zk_layouts::cursor::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
    d[rome_zk_layouts::cursor::OFF_VERSION] = rome_zk_layouts::cursor::VERSION;
    d[rome_zk_layouts::cursor::OFF_CHAIN_ID..rome_zk_layouts::cursor::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
        .copy_from_slice(&next_batch.to_le_bytes());
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    }
}

/// Settlement root account, forward-compatible stand-in: a real `authority` at the layout's own offset,
/// everything else zeroed. `rome_zk_layouts::root::MIN_LEN`/`OFF_CHAIN_ID`/`OFF_AUTHORITY` are the single
/// definitions of this account's shape — this fixture only ever needs `chain_id` and `authority`
/// populated (every consuming test either checks the authority-gated path or ignores the rest of the
/// account).
pub fn root_account_with_authority(chain_id: u64, authority: &Pubkey, owner: Pubkey) -> Account {
    let mut d = vec![0u8; rome_zk_layouts::root::MIN_LEN];
    d[rome_zk_layouts::root::OFF_MAGIC..rome_zk_layouts::root::OFF_MAGIC + 4]
        .copy_from_slice(&rome_zk_layouts::root::MAGIC.to_le_bytes());
    d[rome_zk_layouts::root::OFF_CHAIN_ID..rome_zk_layouts::root::OFF_CHAIN_ID + 8]
        .copy_from_slice(&chain_id.to_le_bytes());
    d[rome_zk_layouts::root::OFF_AUTHORITY..rome_zk_layouts::root::OFF_AUTHORITY + 32]
        .copy_from_slice(authority.as_ref());
    Account {
        lamports: rent_exempt(d.len()),
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

/// Sends `ixs` as one transaction — paid by `payer`, additionally signed by `extra_signers` (empty when
/// none are needed) — against a real `solana-program-test` context, and returns the transaction's real,
/// BPF-measured compute units consumed (`process_transaction_with_metadata`, never replay/estimation,
/// which inflates 3-4x) alongside its raw log messages. The one send-and-observe helper every program's
/// and crate's integration suite in this workspace uses, replacing the 10 near-identical copies that used
/// to live in `programs/zk-inbox`, `programs/zk-settlement`, `rome-zk-batcher` and `rome-zk-derive` test
/// files. A caller that only needs the plain success/failure result, or only the CU figure, adapts this
/// return value at its own one call site — never a second implementation.
///
/// Always fetches a *fresh* blockhash (`get_new_latest_blockhash`, not `get_latest_blockhash`) — several
/// tests across this workspace intentionally send two transactions with byte-identical instructions and
/// signers (e.g. "reseal the same idx"), and without a genuinely new blockhash the ledger treats the
/// second as a duplicate of the first (`TransactionError::AlreadyProcessed`) rather than actually
/// re-running the program.
pub async fn send_measuring_cu(
    ctx: &mut ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> (Result<(), TransactionError>, u64, Option<Vec<String>>) {
    let mut signers = vec![payer];
    signers.extend_from_slice(extra_signers);
    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &signers, recent);
    let meta = ctx
        .banks_client
        .process_transaction_with_metadata(tx)
        .await
        .unwrap();
    // A missing measurement must never read as a pass: every CU gate in this workspace is a ceiling
    // (`cu <= budget`), which 0 satisfies trivially, so the one path that could yield 0 -- the runtime
    // returning no metadata -- is a named failure instead of a default.
    //
    // `metadata` is `None` exactly when the bank refused the transaction before committing it
    // (solana-banks-server 4.3.0 `process_transaction_with_metadata_and_context` maps
    // `Bank::process_transaction_with_metadata`'s `Err(_)` to `metadata: None`). The one cause seen in
    // CI was `Err(AccountInUse)`: an earlier `BanksClient::process_transaction` returns as soon as its
    // signature status is visible, which is written during commit, while the banks-server thread still
    // holds that transaction's account locks until its batch drops; the next transaction touching the
    // same writable account then fails `prepare_entry_batch`. `process_transaction_with_metadata` is
    // processed synchronously and releases its locks before replying, so every send in this workspace's
    // tests goes through it (see `send_checked`) and this refusal cannot happen. If it ever does, it is
    // a real fault, so it panics at once with the result instead of retrying.
    let Some(metadata) = meta.metadata else {
        panic!(
            "process_transaction_with_metadata returned no metadata (the bank refused the transaction \
             before committing it); the CU figure is unavailable and must not read as 0. Result: {:?}",
            meta.result
        );
    };
    (
        meta.result,
        metadata.compute_units_consumed,
        Some(metadata.log_messages),
    )
}

/// Sends `tx` and waits for it, the way every non-measuring test send must: through
/// `process_transaction_with_metadata`, never `BanksClient::process_transaction`. The latter returns as
/// soon as the transaction's signature status is visible (written during commit) while the banks-server
/// thread still holds the transaction's account locks, so an immediately following send that touches the
/// same writable account is refused with `AccountInUse`. The metadata call is processed synchronously and
/// releases its locks before it replies. Returns the transaction's own result.
pub async fn send_checked(
    banks_client: &mut BanksClient,
    tx: Transaction,
) -> Result<(), TransactionError> {
    banks_client
        .process_transaction_with_metadata(tx)
        .await
        .unwrap_or_else(|e| panic!("BanksClient::process_transaction_with_metadata: {e}"))
        .result
}

/// A fixed, distinct program id for the zk-inbox program in CU-gate tests — never `Pubkey::new_unique()`.
/// A PDA's bump-seed search depth (and therefore the measured CU of any instruction that derives one)
/// depends on the program id the search runs against; pinning it is what makes the CU figures these
/// tests assert reproducible and comparable run to run.
pub fn fixed_inbox_program_id() -> Pubkey {
    Pubkey::new_from_array([0x1Bu8; 32])
}

/// A fixed, distinct program id for the zk-settlement program in CU-gate tests (moved here from
/// `programs/zk-settlement/tests/registration_revenue.rs`, which pinned it first) — same rationale as
/// [`fixed_inbox_program_id`], and the same byte pattern, so every CU figure already measured against it
/// is unaffected by the move.
pub fn fixed_settlement_program_id() -> Pubkey {
    Pubkey::new_from_array([0x51u8; 32])
}

/// A fixed, distinct program id for the TEST-ONLY stub bridge program
/// (`programs/zk-settlement/tests/fixtures/stub-bridge`) in CU-gate tests — same rationale as
/// [`fixed_inbox_program_id`]: `ConsumeExit`'s `exit_consumer` PDA is derived under this program's own id
/// (`exit_config.bridge_program`, when the stub is the registered bridge), so a `Pubkey::new_unique()` id
/// would make the measured `ConsumeExit` CU vary run to run with that PDA's own bump-seed search depth.
pub fn fixed_stub_bridge_program_id() -> Pubkey {
    Pubkey::new_from_array([0x62u8; 32])
}

/// A fixed, distinct program id for the `programs/zk-bridge` vault (real, deployed program — not a
/// test-only stub) in CU-gate tests — same rationale as [`fixed_inbox_program_id`]: `ReleaseExit`'s own
/// `vault_config`/`vault`/`vault_authority`/`exit_consumer` PDAs are all derived under this program's own
/// id, so a `Pubkey::new_unique()` id would make every measured CU figure vary run to run with those PDAs'
/// own bump-seed search depths.
pub fn fixed_zk_bridge_program_id() -> Pubkey {
    Pubkey::new_from_array([0x73u8; 32])
}

/// A fixed, distinct pubkey standing in for an SPL mint in `zk-bridge` CU-gate tests — never
/// `Pubkey::new_unique()`. Same rationale as [`fixed_inbox_program_id`]: `vault_token_pda`/`recipient_ata`
/// are both seeded (in part) by the mint, so a `Pubkey::new_unique()` mint makes the measured CU of
/// `InitVault`/`ReleaseExit` vary run to run with that PDA's own bump-seed search depth.
pub fn fixed_mint_pubkey() -> Pubkey {
    Pubkey::new_from_array([0x4Du8; 32])
}

/// A fixed, distinct pubkey standing in for an exit's `sol_recipient` in `zk-bridge` CU-gate tests — same
/// rationale as [`fixed_mint_pubkey`]: the recipient's Associated Token Account is seeded (in part) by the
/// recipient, so a `Pubkey::new_unique()` recipient makes the measured `ReleaseExit` CU vary run to run.
pub fn fixed_recipient_pubkey() -> Pubkey {
    Pubkey::new_from_array([0x52u8; 32])
}

/// The real SPL Token program id (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`) — every real-BPF test in
/// this workspace that CPIs into it loads the genuine compiled program from the pinned `spl-token = "=9.0.0"`
/// crate's own source (see `programs/zk-bridge/README.md`), never a stub; this helper just names the
/// well-known id so call sites don't hardcode the string. A plain constant, not a dependency on any
/// `spl-token*` crate — see `zk_bridge::token`'s own module doc for why this workspace hand-rolls the SPL
/// wire format instead of depending on one.
pub fn spl_token_program_id() -> Pubkey {
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
}

/// An attacker's keypair sending the 0-byte rent-exempt minimum to a predictable PDA before the real creator's
/// transaction lands (the griefing class) — the exact primitive every `*_succeeds_even_when_an_attacker_prefunds_*`
/// test reproduces. `ctx` must already have a live blockhash (i.e. `start_with_context` has run).
pub async fn prefund_pda(ctx: &mut ProgramTestContext, pda: Pubkey) {
    let attacker = Keypair::new();
    ctx.set_account(
        &attacker.pubkey(),
        &AccountSharedData::from(Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        }),
    );
    let recent = ctx.get_new_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[solana_system_interface::instruction::transfer(
            &attacker.pubkey(),
            &pda,
            rent_exempt(0),
        )],
        Some(&attacker.pubkey()),
        &[&attacker],
        recent,
    );
    send_checked(&mut ctx.banks_client, tx).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rent_exempt_matches_the_default_rent_schedule() {
        assert_eq!(rent_exempt(0), Rent::default().minimum_balance(0));
        assert_eq!(rent_exempt(200), Rent::default().minimum_balance(200));
    }

    #[test]
    fn sbf_out_dir_points_at_the_workspace_deploy_dir() {
        assert!(sbf_out_dir().ends_with("/target/deploy"));
    }

    #[test]
    fn cursor_account_round_trips_through_rome_zk_layouts() {
        let program_id = Pubkey::new_unique();
        let acc = cursor_account(program_id, 7, 42);
        assert_eq!(acc.owner, program_id);
        let f = rome_zk_layouts::cursor::read(&acc.data).unwrap();
        assert_eq!(f.chain_id, 7);
        assert_eq!(f.next_batch, 42);
    }

    #[test]
    fn root_account_with_authority_round_trips_through_rome_zk_layouts() {
        let owner = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let acc = root_account_with_authority(11, &authority, owner);
        assert_eq!(acc.owner, owner);
        let f = rome_zk_layouts::root::read(&acc.data).unwrap();
        assert_eq!(f.chain_id, 11);
        assert_eq!(f.authority, authority.to_bytes());
    }

    #[test]
    fn funded_keypair_is_a_fresh_keypair_each_call() {
        assert_ne!(funded_keypair().pubkey(), funded_keypair().pubkey());
    }

    /// Distinct, stable, literal (ids are pinned as literals): a fixed program id that silently changed would move
    /// every CU figure asserted against it without any test failing to say so — this is the independent net.
    #[test]
    fn fixed_program_ids_are_distinct_and_stable() {
        assert_ne!(
            fixed_inbox_program_id(),
            fixed_settlement_program_id(),
            "the inbox and settlement fixed ids must never collide"
        );
        assert_ne!(
            fixed_settlement_program_id(),
            fixed_stub_bridge_program_id(),
            "the settlement and stub-bridge fixed ids must never collide"
        );
        assert_ne!(
            fixed_inbox_program_id(),
            fixed_stub_bridge_program_id(),
            "the inbox and stub-bridge fixed ids must never collide"
        );
        assert_eq!(
            fixed_stub_bridge_program_id().to_string(),
            "7d3y2WdzxE7CfsWjkGy3WndkvZcj1EHMkzKJiFPiDecH"
        );
        assert_eq!(
            fixed_inbox_program_id().to_string(),
            "2poys8aGy427mSkhn4oordqbbHgSBiY961ziaGDBAzi6"
        );
        assert_eq!(
            fixed_settlement_program_id().to_string(),
            "6URwbPipuA4MJLG7LCRRZuWnms3JZ9cRG3z9indXWz8G"
        );
        assert_ne!(
            fixed_zk_bridge_program_id(),
            fixed_settlement_program_id(),
            "the bridge and settlement fixed ids must never collide"
        );
        assert_ne!(
            fixed_zk_bridge_program_id(),
            fixed_stub_bridge_program_id(),
            "the real bridge and the test-only stub bridge fixed ids must never collide"
        );
        assert_ne!(
            fixed_zk_bridge_program_id(),
            fixed_inbox_program_id(),
            "the bridge and inbox fixed ids must never collide"
        );
    }

    #[test]
    fn spl_token_program_id_is_the_well_known_mainnet_address() {
        assert_eq!(
            spl_token_program_id().to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
    }

    #[test]
    fn fixed_mint_and_recipient_are_distinct_and_stable() {
        assert_ne!(fixed_mint_pubkey(), fixed_recipient_pubkey());
        assert_ne!(fixed_mint_pubkey(), fixed_zk_bridge_program_id());
        assert_ne!(fixed_recipient_pubkey(), fixed_zk_bridge_program_id());
        assert_eq!(fixed_mint_pubkey(), fixed_mint_pubkey());
        assert_eq!(fixed_recipient_pubkey(), fixed_recipient_pubkey());
    }
}
