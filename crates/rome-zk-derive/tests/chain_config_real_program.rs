//! Contract: `chain_bound::chain_drift_bound` proved against the REAL,
//! `cargo build-sbf`-compiled `zk_settlement.so` — not a hand-built account, not the `FakeAccountReader`
//! `src/chain_bound.rs`'s own unit tests use. `InitChainV2` is the only real producer of a v2
//! `chain_config` account; this test drives it (via `InitGlobalConfig` + `AllowReservedId`, the reserved
//! chain-id path — the rig `programs/zk-settlement/tests/registration_revenue.rs` already exercises in
//! full) with `max_drift_secs = 45` (not 60, so a stray hardcoded default cannot pass this by accident),
//! then reads the resulting `chain_config` back through [`chain_drift_bound`] over a real
//! `solana-program-test` `BanksClient` — the same [`AccountReader`] seam `tests/real_program_inbox.rs`
//! proves [`rome_zk_derive::inbox::InboxRetrieval`] against.

use rome_zk_derive::chain_bound::chain_drift_bound;
use rome_zk_derive::reader::AccountReader;
use rome_zk_derive::PipelineError;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
};
use solana_system_interface::program as system_program;
use zk_settlement_client as sclient;

const CHAIN_ID: u64 = 200_198;
const MAX_DRIFT_SECS: u64 = 45;

fn funded_account_with(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// [`AccountReader`] over a real `BanksClient` — the same shape
/// `tests/real_program_inbox.rs::BanksAccountReader` uses, duplicated here rather than shared (that
/// struct is private to its own test binary, same convention `programs/zk-settlement/tests/
/// registration_revenue.rs`'s header doc already documents for this repo's rig helpers).
#[derive(Clone)]
struct BanksAccountReader(solana_program_test::BanksClient);

impl AccountReader for BanksAccountReader {
    async fn get_account_data(&mut self, pubkey: Pubkey) -> Result<Option<Vec<u8>>, PipelineError> {
        self.0
            .get_account(pubkey)
            .await
            .map(|opt| opt.map(|a| a.data))
            .map_err(|e| PipelineError::Temporary(format!("get_account: {e}")))
    }
}

async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) {
    rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers)
        .await
        .0
        .unwrap_or_else(|e| panic!("tx failed: {e:?}"));
}

/// One reserved chain, fully registered with `max_drift_secs = 45` — the minimal real-program rig this
/// test needs (`InitGlobalConfig` -> `AllowReservedId` -> `InitChainV2`), trimmed from
/// `programs/zk-settlement/tests/registration_revenue.rs`'s own `rig`/`rig_program_test` (that file's
/// full deposit/fee/reclaim lifecycle rig is more than this test needs).
async fn registered_chain() -> (
    solana_program_test::ProgramTestContext,
    Pubkey,
    Keypair,
    Keypair,
) {
    let settlement_program = Pubkey::new_unique();
    let payer = rome_zk_testkit::funded_keypair();
    let authority = rome_zk_testkit::funded_keypair();
    let registry_authority = rome_zk_testkit::funded_keypair();
    let treasury = Pubkey::new_unique();
    let upgrade_authority = rome_zk_testkit::funded_keypair();

    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_settlement",
            settlement_program,
        )],
        true,
    );
    pt.add_account(payer.pubkey(), funded_account_with(50_000_000_000));
    pt.add_account(authority.pubkey(), funded_account_with(50_000_000_000));
    pt.add_account(treasury, funded_account_with(10_000_000));
    let mut ctx = pt.start_with_context().await;

    // `ProgramSpec::upgradeable` seeds the ProgramData account with authority
    // `Pubkey::default()` (`add_upgradeable_program_to_genesis`) — patch it to `upgrade_authority` before
    // signing `InitGlobalConfig` with it, same as `registration_revenue.rs`'s own
    // `start_and_init_global_config`.
    let program_data = sclient::program_data_pda(&settlement_program);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist");
    pd_account.data[13..45].copy_from_slice(upgrade_authority.pubkey().as_ref());
    ctx.set_account(&program_data, &pd_account.into());

    let global_fields = sclient::GlobalConfigFields {
        registry_authority: registry_authority.pubkey(),
        treasury,
        permissionless_init_enabled: false,
        reclaim_window_slots: zk_settlement::governance::MIN_RECLAIM_WINDOW_SLOTS,
        deposit_lamports: 2_000_000_000,
        default_fee_base_lamports: 1_000_000,
        default_fee_bps: 0,
    };
    let init_global_ix = sclient::init_global_config_ix(
        &settlement_program,
        &payer.pubkey(),
        &upgrade_authority.pubkey(),
        global_fields,
    );
    send(&mut ctx, &[init_global_ix], &payer, &[&upgrade_authority]).await;

    let allow_ix = sclient::allow_reserved_id_ix(
        &settlement_program,
        &payer.pubkey(),
        &registry_authority.pubkey(),
        CHAIN_ID,
    );
    send(&mut ctx, &[allow_ix], &payer, &[&registry_authority]).await;

    let init_chain_ix = sclient::init_chain_reserved_ix(
        &settlement_program,
        &payer.pubkey(),
        &authority.pubkey(),
        &registry_authority.pubkey(),
        CHAIN_ID,
        sclient::InitChainFields {
            number: 0,
            parent_hash: [0u8; 32],
            state_root: [0u8; 32],
            block_hash: [0u8; 32],
            profile: 0,
            challenge_window_slots: 5,
            prove_window_slots: 1000,
            proving_policy: 1,
            poster_bond: 0,
            exit_cap_per_window: 0,
            max_pending: 16,
            inbox_program: Pubkey::new_unique(),
            registry_entries: vec![],
            max_drift_secs: MAX_DRIFT_SECS,
        },
    );
    send(
        &mut ctx,
        &[init_chain_ix],
        &payer,
        &[&authority, &registry_authority],
    )
    .await;

    (ctx, settlement_program, authority, registry_authority)
}

#[tokio::test]
async fn chain_drift_bound_over_a_real_init_chain_v2_account_returns_its_max_drift_secs() {
    let (ctx, settlement_program, _authority, _registry_authority) = registered_chain().await;

    // Sanity: the real program actually wrote a v2 `chain_config` naming 45 — failing here would mean
    // the rig above is wrong, not that `chain_drift_bound` is.
    let (cc_pda, _) = sclient::chain_config_pda(&settlement_program, CHAIN_ID);
    let cc_account = ctx
        .banks_client
        .get_account(cc_pda)
        .await
        .unwrap()
        .expect("chain_config account must exist after InitChainV2");
    let decoded = sclient::decode_chain_config_account(&cc_account.data).unwrap();
    assert_eq!(decoded.max_drift_secs, Some(MAX_DRIFT_SECS));

    // --- the actual seam under test: chain_drift_bound, over a real BanksClient-backed AccountReader ---
    let mut reader = BanksAccountReader(ctx.banks_client.clone());
    let bound = chain_drift_bound(&mut reader, &settlement_program, CHAIN_ID)
        .await
        .expect("a freshly InitChainV2'd chain_config must yield a bound");
    assert_eq!(
        bound, MAX_DRIFT_SECS,
        "chain_drift_bound must return the real program's own committed max_drift_secs, not any \
         hardcoded default"
    );
}
