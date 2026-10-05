//! Real-BPF tests for the deposit setup builders in `zk-bridge-client`: `init_bridge_config_ix`,
//! `init_deposit_queue_ix`, `propose_deposit_params_ix` and `activate_deposit_params_ix`. Each builder's
//! instruction is sent to the real program and must succeed, and each has one refusal checked by name, so a
//! builder that drifts from the program's account list fails here. The `.so` files are the `cargo build-sbf`
//! output. The settlement `root`, the `registry` and the `vault_config` are fixtures built as accounts.
//!
//! `deposit.rs` builds every `Deposit` and `CloseDeposit` it sends with the client's `deposit_ix` and
//! `close_deposit_ix`, so those two are covered there against the program as it is after the registry check.

mod common;

use common::{bridge_program_id, funded_account, get_account, settlement_program_id};
use rome_zk_layouts::deposit_queue::{bridge_config, deposit_queue as queue_layout};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use zk_bridge::errors::BridgeError;
use zk_bridge_client::DepositParamsArgs;

/// A permissionless chain id (at or above 2^32), so `InitDepositQueue` does not refuse it as reserved.
const CHAIN: u64 = (1u64 << 32) + 11;
const CHALLENGE_WINDOW: u32 = 50;

fn inbox_program_id() -> Pubkey {
    rome_zk_testkit::fixed_inbox_program_id()
}

fn fee_recipient_key() -> Pubkey {
    Pubkey::new_from_array([0xfe; 32])
}

fn params(max_per_block: u16) -> DepositParamsArgs {
    DepositParamsArgs {
        inclusion_deadline_secs: 43_200,
        max_per_batch: 256,
        max_per_block,
        min_amount: 1_000_000,
        fee_lamports: 100_000,
        fee_recipient: fee_recipient_key(),
    }
}

fn owned(owner: Pubkey, data: Vec<u8>) -> Account {
    Account {
        lamports: rome_zk_testkit::rent_exempt(data.len()),
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

fn config_account() -> Account {
    let mut d = vec![0u8; bridge_config::LEN];
    bridge_config::write(
        &mut d,
        &bridge_config::BridgeConfigFields {
            settlement_program: settlement_program_id().to_bytes(),
            inbox_program: inbox_program_id().to_bytes(),
        },
    );
    owned(bridge_program_id(), d)
}

fn root_account(authority: &Pubkey, head_pending_batch: u64) -> Account {
    let mut a =
        rome_zk_testkit::root_account_with_authority(CHAIN, authority, settlement_program_id());
    let o = rome_zk_layouts::root::OFF_CHALLENGE_WINDOW_SLOTS;
    a.data[o..o + 4].copy_from_slice(&CHALLENGE_WINDOW.to_le_bytes());
    let o = rome_zk_layouts::root::OFF_HEAD_PENDING_BATCH;
    a.data[o..o + 8].copy_from_slice(&head_pending_batch.to_le_bytes());
    a
}

fn registry_account() -> Account {
    use rome_zk_layouts::registry as r;
    let mut d = vec![0u8; r::REGISTRY_LEN];
    d[r::OFF_MAGIC..r::OFF_MAGIC + 4].copy_from_slice(&r::MAGIC.to_le_bytes());
    d[r::OFF_CHAIN_ID..r::OFF_CHAIN_ID + 8].copy_from_slice(&CHAIN.to_le_bytes());
    d[r::OFF_INBOX_PROGRAM..r::OFF_INBOX_PROGRAM + 32].copy_from_slice(inbox_program_id().as_ref());
    owned(settlement_program_id(), d)
}

fn vault_account() -> Account {
    let d =
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id: CHAIN,
            settlement_program: settlement_program_id(),
            mint: rome_zk_testkit::fixed_mint_pubkey(),
            mint_decimals: 9,
            authority: Pubkey::new_unique(),
        })
        .to_vec();
    owned(bridge_program_id(), d)
}

/// A queue holding `live`, with `pending` and its activation slot when given.
fn queue_account(live: &DepositParamsArgs, pending: Option<(&DepositParamsArgs, u64)>) -> Account {
    let layout = |p: &DepositParamsArgs| queue_layout::DepositParams {
        inclusion_deadline_secs: p.inclusion_deadline_secs,
        max_per_batch: p.max_per_batch,
        max_per_block: p.max_per_block,
        min_amount: p.min_amount,
        fee_lamports: p.fee_lamports,
        fee_recipient: p.fee_recipient.to_bytes(),
    };
    let mut d = vec![0u8; queue_layout::LEN];
    queue_layout::write(
        &mut d,
        &queue_layout::DepositQueueFields {
            count: 0,
            head_hash: [0x99; 32],
            params: layout(live),
            pending: pending.map(|p| layout(p.0)).unwrap_or_default(),
            activation_slot: pending.map(|p| p.1).unwrap_or(0),
        },
    );
    owned(bridge_program_id(), d)
}

fn assert_custom(result: Result<(), TransactionError>, want: BridgeError) {
    match result {
        Err(TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        )) => assert_eq!(c, want as u32, "expected {want:?}, got Custom({c})"),
        other => panic!("expected {want:?}, got {other:?}"),
    }
}

fn queue_key() -> Pubkey {
    zk_bridge_client::deposit_queue_pda(&bridge_program_id(), &settlement_program_id(), CHAIN).0
}

struct Rig {
    payer: Keypair,
    authority: Keypair,
    head_pending_batch: u64,
    queue: Option<Account>,
}

impl Rig {
    fn new() -> Self {
        Rig {
            payer: Keypair::new(),
            authority: Keypair::new(),
            head_pending_batch: 1,
            queue: None,
        }
    }

    async fn start(&self) -> ProgramTestContext {
        let mut pt = rome_zk_testkit::program_test(
            &[rome_zk_testkit::ProgramSpec::upgradeable(
                "zk_bridge",
                bridge_program_id(),
            )],
            true,
        );
        pt.add_account(self.payer.pubkey(), funded_account(50_000_000_000));
        pt.add_account(self.authority.pubkey(), funded_account(1_000_000_000));
        pt.add_account(
            zk_bridge_client::bridge_config_pda(&bridge_program_id()).0,
            config_account(),
        );
        pt.add_account(
            rome_zk_layouts::root::pda(&settlement_program_id(), CHAIN).0,
            root_account(&self.authority.pubkey(), self.head_pending_batch),
        );
        pt.add_account(
            rome_zk_layouts::registry::pda(&settlement_program_id(), CHAIN).0,
            registry_account(),
        );
        pt.add_account(
            zk_bridge_client::vault_config_pda(
                &bridge_program_id(),
                &settlement_program_id(),
                CHAIN,
            )
            .0,
            vault_account(),
        );
        pt.add_account(
            fee_recipient_key(),
            funded_account(rome_zk_testkit::rent_exempt(0)),
        );
        if let Some(q) = &self.queue {
            pt.add_account(queue_key(), q.clone());
        }
        pt.start_with_context().await
    }

    async fn send(
        &self,
        ctx: &mut ProgramTestContext,
        ix: Instruction,
        signers: &[&Keypair],
        name: &str,
    ) -> Result<(), TransactionError> {
        let (r, cu, _) = rome_zk_testkit::send_measuring_cu(ctx, &[ix], &self.payer, signers).await;
        eprintln!("{name} consumed {cu} CU");
        r
    }
}

async fn read_queue(ctx: &mut ProgramTestContext) -> queue_layout::DepositQueueFields {
    let a = get_account(ctx, queue_key())
        .await
        .expect("queue must exist");
    queue_layout::read(&a.data).unwrap()
}

// ---- init_bridge_config_ix ----

/// Starts a rig whose bridge `ProgramData` names `upgrade_authority`, with no bridge config yet.
async fn start_for_bridge_config(rig: &Rig, upgrade_authority: &Keypair) -> ProgramTestContext {
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::upgradeable(
            "zk_bridge",
            bridge_program_id(),
        )],
        true,
    );
    pt.add_account(rig.payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(upgrade_authority.pubkey(), funded_account(1_000_000_000));
    let mut ctx = pt.start_with_context().await;
    let pd = zk_bridge_client::bridge_program_data(&bridge_program_id());
    let mut a = ctx
        .banks_client
        .get_account(pd)
        .await
        .unwrap()
        .expect("ProgramData");
    a.data[13..45].copy_from_slice(upgrade_authority.pubkey().as_ref());
    ctx.set_account(&pd, &a.into());
    ctx
}

#[tokio::test]
async fn init_bridge_config_ix_is_accepted_and_writes_the_two_programs() {
    let rig = Rig::new();
    let upgrade_authority = Keypair::new();
    let mut ctx = start_for_bridge_config(&rig, &upgrade_authority).await;
    let ix = zk_bridge_client::init_bridge_config_ix(
        &bridge_program_id(),
        &rig.payer.pubkey(),
        &upgrade_authority.pubkey(),
        &settlement_program_id(),
        &inbox_program_id(),
    );
    rig.send(&mut ctx, ix, &[&upgrade_authority], "InitBridgeConfig")
        .await
        .expect("the client's InitBridgeConfig must succeed under the upgrade authority");
    let a = get_account(
        &mut ctx,
        zk_bridge_client::bridge_config_pda(&bridge_program_id()).0,
    )
    .await
    .expect("config must exist");
    let f = bridge_config::read(&a.data).unwrap();
    assert_eq!(f.settlement_program, settlement_program_id().to_bytes());
    assert_eq!(f.inbox_program, inbox_program_id().to_bytes());
}

#[tokio::test]
async fn init_bridge_config_ix_is_refused_for_a_signer_that_is_not_the_upgrade_authority() {
    let rig = Rig::new();
    let upgrade_authority = Keypair::new();
    let mut ctx = start_for_bridge_config(&rig, &upgrade_authority).await;
    let impostor = Keypair::new();
    ctx.set_account(&impostor.pubkey(), &funded_account(1_000_000_000).into());
    let ix = zk_bridge_client::init_bridge_config_ix(
        &bridge_program_id(),
        &rig.payer.pubkey(),
        &impostor.pubkey(),
        &settlement_program_id(),
        &inbox_program_id(),
    );
    let r = rig
        .send(&mut ctx, ix, &[&impostor], "InitBridgeConfig (impostor)")
        .await;
    assert_custom(r, BridgeError::NotUpgradeAuthority);
}

// ---- init_deposit_queue_ix ----

fn init_queue_ix(rig: &Rig, p: DepositParamsArgs) -> Instruction {
    zk_bridge_client::init_deposit_queue_ix(
        &bridge_program_id(),
        &rig.payer.pubkey(),
        &rig.authority.pubkey(),
        &settlement_program_id(),
        CHAIN,
        p,
    )
}

#[tokio::test]
async fn init_deposit_queue_ix_is_accepted_and_writes_the_parameters() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    rig.send(
        &mut ctx,
        init_queue_ix(&rig, params(4)),
        &[&rig.authority],
        "InitDepositQueue",
    )
    .await
    .expect("the client's InitDepositQueue must succeed");
    let q = read_queue(&mut ctx).await;
    assert_eq!(q.count, 0);
    assert_eq!(q.params.inclusion_deadline_secs, 43_200);
    assert_eq!(q.params.max_per_batch, 256);
    assert_eq!(q.params.max_per_block, 4);
    assert_eq!(q.params.min_amount, 1_000_000);
    assert_eq!(q.params.fee_lamports, 100_000);
    assert_eq!(q.params.fee_recipient, fee_recipient_key().to_bytes());
}

#[tokio::test]
async fn init_deposit_queue_ix_is_refused_for_a_chain_that_never_posted() {
    let mut rig = Rig::new();
    rig.head_pending_batch = 0;
    let mut ctx = rig.start().await;
    let r = rig
        .send(
            &mut ctx,
            init_queue_ix(&rig, params(4)),
            &[&rig.authority],
            "InitDepositQueue (never posted)",
        )
        .await;
    assert_custom(r, BridgeError::ChainNeverPosted);
    assert!(get_account(&mut ctx, queue_key()).await.is_none());
}

// ---- propose_deposit_params_ix ----

fn propose_ix(rig: &Rig, slot: u64, p: DepositParamsArgs) -> Instruction {
    zk_bridge_client::propose_deposit_params_ix(
        &bridge_program_id(),
        &rig.authority.pubkey(),
        &settlement_program_id(),
        CHAIN,
        slot,
        p,
    )
}

#[tokio::test]
async fn propose_deposit_params_ix_is_accepted_and_records_the_pending_set() {
    let mut rig = Rig::new();
    rig.queue = Some(queue_account(&params(4), None));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let slot = 100 + CHALLENGE_WINDOW as u64;
    rig.send(
        &mut ctx,
        propose_ix(&rig, slot, params(2)),
        &[&rig.authority],
        "ProposeDepositParams",
    )
    .await
    .expect("the client's ProposeDepositParams must succeed");
    let q = read_queue(&mut ctx).await;
    assert_eq!(q.params.max_per_block, 4, "the live parameters do not move");
    assert_eq!(q.pending.max_per_block, 2);
    assert_eq!(q.activation_slot, slot);
}

#[tokio::test]
async fn propose_deposit_params_ix_is_refused_under_one_challenge_window() {
    let mut rig = Rig::new();
    rig.queue = Some(queue_account(&params(4), None));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let r = rig
        .send(
            &mut ctx,
            propose_ix(&rig, 100 + CHALLENGE_WINDOW as u64 - 1, params(2)),
            &[&rig.authority],
            "ProposeDepositParams (too soon)",
        )
        .await;
    assert_custom(r, BridgeError::ActivationTooSoon);
}

// ---- activate_deposit_params_ix ----

fn activate_ix() -> Instruction {
    zk_bridge_client::activate_deposit_params_ix(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN,
        &fee_recipient_key(),
    )
}

#[tokio::test]
async fn activate_deposit_params_ix_is_accepted_at_its_slot_and_refused_before_it() {
    let mut rig = Rig::new();
    let pending = params(2);
    rig.queue = Some(queue_account(&params(4), Some((&pending, 200))));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(199).unwrap();
    let r = rig
        .send(
            &mut ctx,
            activate_ix(),
            &[],
            "ActivateDepositParams (early)",
        )
        .await;
    assert_custom(r, BridgeError::ActivationNotReached);

    ctx.warp_to_slot(200).unwrap();
    rig.send(&mut ctx, activate_ix(), &[], "ActivateDepositParams")
        .await
        .expect("the client's ActivateDepositParams must succeed at its slot");
    let q = read_queue(&mut ctx).await;
    assert_eq!(q.params.max_per_block, 2);
    assert_eq!(q.activation_slot, 0);
}
