//! Real-BPF tests for the deposit setup instructions: `InitBridgeConfig`, `InitDepositQueue`,
//! `ProposeDepositParams` and `ActivateDepositParams`. The `.so` files are the `cargo build-sbf` output
//! (build them first with `cargo build-sbf --arch v3` for each program). The settlement `root`, the `registry` and the `vault_config` are
//! fixtures built as accounts, the way `vault.rs` builds its settlement accounts; every instruction under
//! test runs as the real program.
//!
//! The instruction builders live in this file, not in `zk-bridge-client`, which a separate change owns.

mod common;

use borsh::to_vec;
use common::{bridge_program_id, funded_account, get_account, settlement_program_id};
use rome_zk_layouts::deposit_queue::{
    bridge_config, deposit_queue as queue_layout, deposit_queue::DepositParams,
};
use solana_program::{
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    keccak,
    pubkey::Pubkey,
};
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_bridge::errors::BridgeError;
use zk_bridge::instruction::{
    ActivateDepositParamsArgs, BridgeIx, DepositParamsArgs, InitBridgeConfigArgs,
    InitDepositQueueArgs, ProposeDepositParamsArgs,
};

/// A permissionless chain id (at or above 2^32), so `InitDepositQueue` does not refuse it as reserved.
const CHAIN: u64 = (1u64 << 32) + 7;
const CHALLENGE_WINDOW: u32 = 50;

fn inbox_program_id() -> Pubkey {
    rome_zk_testkit::fixed_inbox_program_id()
}
fn fee_recipient_key() -> Pubkey {
    Pubkey::new_from_array([0xfe; 32])
}
fn soft_keccak(parts: &[&[u8]]) -> [u8; 32] {
    keccak::hashv(parts).to_bytes()
}

fn default_params() -> DepositParamsArgs {
    DepositParamsArgs {
        inclusion_deadline_secs: 43_200,
        max_per_batch: 64,
        max_per_block: 8,
        min_amount: 1_000,
        fee_lamports: 1_000_000,
        fee_recipient: fee_recipient_key(),
    }
}

fn to_layout(p: &DepositParamsArgs) -> DepositParams {
    DepositParams {
        inclusion_deadline_secs: p.inclusion_deadline_secs,
        max_per_batch: p.max_per_batch,
        max_per_block: p.max_per_block,
        min_amount: p.min_amount,
        fee_lamports: p.fee_lamports,
        fee_recipient: p.fee_recipient.to_bytes(),
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

fn bridge_config_account(owner: Pubkey, settlement: Pubkey, inbox: Pubkey) -> Account {
    let mut d = vec![0u8; bridge_config::LEN];
    bridge_config::write(
        &mut d,
        &bridge_config::BridgeConfigFields {
            settlement_program: settlement.to_bytes(),
            inbox_program: inbox.to_bytes(),
        },
    );
    owned(owner, d)
}

fn root_account(chain_id: u64, authority: &Pubkey, owner: Pubkey, window: u32) -> Account {
    let mut a = rome_zk_testkit::root_account_with_authority(chain_id, authority, owner);
    let o = rome_zk_layouts::root::OFF_CHALLENGE_WINDOW_SLOTS;
    a.data[o..o + 4].copy_from_slice(&window.to_le_bytes());
    // The chain has posted: a queue is only set up for a chain that has.
    let o = rome_zk_layouts::root::OFF_HEAD_PENDING_BATCH;
    a.data[o..o + 8].copy_from_slice(&1u64.to_le_bytes());
    a
}

/// The same root for a chain that has never posted.
fn never_posted_root(chain_id: u64, authority: &Pubkey, owner: Pubkey, window: u32) -> Account {
    let mut a = root_account(chain_id, authority, owner, window);
    let o = rome_zk_layouts::root::OFF_HEAD_PENDING_BATCH;
    a.data[o..o + 8].copy_from_slice(&0u64.to_le_bytes());
    a
}

fn registry_account(chain_id: u64, inbox: &Pubkey, owner: Pubkey) -> Account {
    use rome_zk_layouts::registry as r;
    let mut d = vec![0u8; r::REGISTRY_LEN];
    d[r::OFF_MAGIC..r::OFF_MAGIC + 4].copy_from_slice(&r::MAGIC.to_le_bytes());
    d[r::OFF_CHAIN_ID..r::OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
    d[r::OFF_INBOX_PROGRAM..r::OFF_INBOX_PROGRAM + 32].copy_from_slice(inbox.as_ref());
    owned(owner, d)
}

fn vault_account(chain_id: u64, decimals: u8, owner: Pubkey) -> Account {
    let d =
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id,
            settlement_program: settlement_program_id(),
            mint: rome_zk_testkit::fixed_mint_pubkey(),
            mint_decimals: decimals,
            authority: Pubkey::new_unique(),
        })
        .to_vec();
    owned(owner, d)
}

fn queue_account(
    owner: Pubkey,
    params: &DepositParamsArgs,
    pending: Option<(&DepositParamsArgs, u64)>,
) -> Account {
    let mut d = vec![0u8; queue_layout::LEN];
    queue_layout::write(
        &mut d,
        &queue_layout::DepositQueueFields {
            count: 0,
            head_hash: [0x99; 32],
            params: to_layout(params),
            pending: pending.map(|p| to_layout(p.0)).unwrap_or_default(),
            activation_slot: pending.map(|p| p.1).unwrap_or(0),
        },
    );
    owned(owner, d)
}

fn fee_recipient_account(lamports: u64) -> Account {
    funded_account(lamports)
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

/// Every account and argument one setup instruction reads, with working defaults. A test changes the one
/// field its refusal is about and leaves the rest valid.
struct Rig {
    payer: Keypair,
    authority: Keypair,
    chain_id: u64,
    config: Option<(Pubkey, Account)>,
    root: (Pubkey, Account),
    registry: (Pubkey, Account),
    vault: Option<(Pubkey, Account)>,
    fee_recipient: (Pubkey, Account),
    queue: (Pubkey, Option<Account>),
    /// Who signs as chain authority in the instruction.
    signer: Keypair,
    settlement_arg: Pubkey,
    params: DepositParamsArgs,
}

impl Rig {
    /// Everything valid for `InitDepositQueue` on `CHAIN`; no queue exists yet.
    fn new() -> Self {
        let bridge = bridge_program_id();
        let settlement = settlement_program_id();
        let authority = Keypair::new();
        let signer = authority.insecure_clone();
        let chain_id = CHAIN;
        Rig {
            payer: Keypair::new(),
            chain_id,
            config: Some((
                bridge_config::pda(&bridge).0,
                bridge_config_account(bridge, settlement, inbox_program_id()),
            )),
            root: (
                rome_zk_layouts::root::pda(&settlement, chain_id).0,
                root_account(chain_id, &authority.pubkey(), settlement, CHALLENGE_WINDOW),
            ),
            registry: (
                rome_zk_layouts::registry::pda(&settlement, chain_id).0,
                registry_account(chain_id, &inbox_program_id(), settlement),
            ),
            vault: Some((
                zk_bridge::state::vault_config_pda(&bridge, &settlement, chain_id).0,
                vault_account(chain_id, 9, bridge),
            )),
            fee_recipient: (fee_recipient_key(), fee_recipient_account(1_000_000)),
            queue: (
                queue_layout::pda(&bridge, &settlement.to_bytes(), chain_id).0,
                None,
            ),
            signer,
            authority,
            settlement_arg: settlement,
            params: default_params(),
        }
    }

    /// The same, with a live queue holding the default parameters, for propose and activate.
    fn with_queue() -> Self {
        let mut r = Self::new();
        r.queue.1 = Some(queue_account(bridge_program_id(), &default_params(), None));
        r
    }

    /// A rig with the active params in the queue set to `p`.
    fn set_queue(&mut self, pending: Option<(&DepositParamsArgs, u64)>) {
        self.queue.1 = Some(queue_account(
            bridge_program_id(),
            &default_params(),
            pending,
        ));
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
        pt.add_account(self.signer.pubkey(), funded_account(1_000_000_000));
        if let Some((k, a)) = &self.config {
            pt.add_account(*k, a.clone());
        }
        pt.add_account(self.root.0, self.root.1.clone());
        pt.add_account(self.registry.0, self.registry.1.clone());
        if let Some((k, a)) = &self.vault {
            pt.add_account(*k, a.clone());
        }
        pt.add_account(self.fee_recipient.0, self.fee_recipient.1.clone());
        if let Some(a) = &self.queue.1 {
            pt.add_account(self.queue.0, a.clone());
        }
        pt.start_with_context().await
    }

    fn init_ix(&self) -> Instruction {
        let b = |o: &Option<(Pubkey, Account)>| o.as_ref().map(|x| x.0).unwrap_or_default();
        Instruction {
            program_id: bridge_program_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.signer.pubkey(), true),
                AccountMeta::new_readonly(b(&self.config), false),
                AccountMeta::new_readonly(self.root.0, false),
                AccountMeta::new_readonly(self.registry.0, false),
                AccountMeta::new_readonly(b(&self.vault), false),
                AccountMeta::new_readonly(self.fee_recipient.0, false),
                AccountMeta::new(self.queue.0, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
            data: to_vec(&BridgeIx::InitDepositQueue(InitDepositQueueArgs {
                chain_id: self.chain_id,
                settlement_program: self.settlement_arg,
                params: self.params,
            }))
            .unwrap(),
        }
    }

    fn propose_ix(&self, activation_slot: u64) -> Instruction {
        Instruction {
            program_id: bridge_program_id(),
            accounts: vec![
                AccountMeta::new_readonly(self.signer.pubkey(), true),
                AccountMeta::new_readonly(self.config.as_ref().unwrap().0, false),
                AccountMeta::new_readonly(self.root.0, false),
                AccountMeta::new(self.queue.0, false),
                AccountMeta::new_readonly(self.fee_recipient.0, false),
            ],
            data: to_vec(&BridgeIx::ProposeDepositParams(ProposeDepositParamsArgs {
                chain_id: self.chain_id,
                activation_slot,
                params: self.params,
            }))
            .unwrap(),
        }
    }

    fn activate_ix(&self) -> Instruction {
        Instruction {
            program_id: bridge_program_id(),
            accounts: vec![
                AccountMeta::new_readonly(self.config.as_ref().unwrap().0, false),
                AccountMeta::new(self.queue.0, false),
                AccountMeta::new_readonly(self.fee_recipient.0, false),
            ],
            data: to_vec(&BridgeIx::ActivateDepositParams(
                ActivateDepositParamsArgs {
                    chain_id: self.chain_id,
                },
            ))
            .unwrap(),
        }
    }

    async fn send(
        &self,
        ctx: &mut ProgramTestContext,
        ix: Instruction,
        name: &str,
    ) -> Result<(), TransactionError> {
        let needs_signer = ix
            .accounts
            .iter()
            .any(|m| m.is_signer && m.pubkey == self.signer.pubkey());
        let extra: Vec<&Keypair> = if needs_signer {
            vec![&self.signer]
        } else {
            vec![]
        };
        let (r, cu, _) = rome_zk_testkit::send_measuring_cu(ctx, &[ix], &self.payer, &extra).await;
        eprintln!("{name} consumed {cu} CU");
        r
    }

    /// Runs `InitDepositQueue` and asserts the named refusal and that the queue address is exactly as it was.
    async fn init_refused(&self, want: BridgeError) {
        let mut ctx = self.start().await;
        let before = get_account(&mut ctx, self.queue.0).await;
        let r = self
            .send(&mut ctx, self.init_ix(), "InitDepositQueue (refused)")
            .await;
        assert_custom(r, want);
        let after = get_account(&mut ctx, self.queue.0).await;
        assert_eq!(
            before.map(|a| (a.owner, a.data, a.lamports)),
            after.map(|a| (a.owner, a.data, a.lamports)),
            "a refused InitDepositQueue must leave the queue address as it was"
        );
    }

    /// Runs `ProposeDepositParams` at `activation_slot` (after warping to slot 100) and asserts the refusal.
    async fn propose_refused(&self, activation_slot: u64, want: BridgeError) {
        let mut ctx = self.start().await;
        ctx.warp_to_slot(100).unwrap();
        let r = self
            .send(
                &mut ctx,
                self.propose_ix(activation_slot),
                "ProposeDepositParams (refused)",
            )
            .await;
        assert_custom(r, want);
        let q = get_account(&mut ctx, self.queue.0).await.unwrap();
        let f = queue_layout::read(&q.data).unwrap();
        assert_eq!(
            f.activation_slot, 0,
            "a refused proposal must write nothing"
        );
    }
}

async fn read_queue(ctx: &mut ProgramTestContext, key: Pubkey) -> queue_layout::DepositQueueFields {
    let a = get_account(ctx, key).await.expect("queue must exist");
    assert_eq!(a.owner, bridge_program_id());
    assert_eq!(a.data.len(), queue_layout::LEN);
    queue_layout::read(&a.data).unwrap()
}

async fn now_slot(ctx: &mut ProgramTestContext) -> u64 {
    ctx.banks_client.get_sysvar::<Clock>().await.unwrap().slot
}

// ------------------------------------------------------------------------------------------------
// InitBridgeConfig
// ------------------------------------------------------------------------------------------------

struct CfgRig {
    payer: Keypair,
    upgrade_authority: Keypair,
}

impl CfgRig {
    fn new() -> Self {
        CfgRig {
            payer: Keypair::new(),
            upgrade_authority: Keypair::new(),
        }
    }

    async fn start(&self, prefund_config: bool) -> ProgramTestContext {
        let mut pt = rome_zk_testkit::program_test(
            &[rome_zk_testkit::ProgramSpec::upgradeable(
                "zk_bridge",
                bridge_program_id(),
            )],
            true,
        );
        pt.add_account(self.payer.pubkey(), funded_account(50_000_000_000));
        pt.add_account(
            self.upgrade_authority.pubkey(),
            funded_account(1_000_000_000),
        );
        if prefund_config {
            pt.add_account(
                bridge_config::pda(&bridge_program_id()).0,
                funded_account(rome_zk_testkit::rent_exempt(bridge_config::LEN)),
            );
        }
        let mut ctx = pt.start_with_context().await;
        // Point the ProgramData account's stored authority at this rig's upgrade authority.
        let pd = solana_loader_v3_interface::get_program_data_address(&bridge_program_id());
        let mut a = ctx
            .banks_client
            .get_account(pd)
            .await
            .unwrap()
            .expect("ProgramData");
        a.data[13..45].copy_from_slice(self.upgrade_authority.pubkey().as_ref());
        ctx.set_account(&pd, &a.into());
        ctx
    }

    fn ix(
        &self,
        signer: &Pubkey,
        program_data: Pubkey,
        settlement: Pubkey,
        inbox: Pubkey,
    ) -> Instruction {
        Instruction {
            program_id: bridge_program_id(),
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(*signer, true),
                AccountMeta::new(bridge_config::pda(&bridge_program_id()).0, false),
                AccountMeta::new_readonly(program_data, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
            data: to_vec(&BridgeIx::InitBridgeConfig(InitBridgeConfigArgs {
                settlement_program: settlement,
                inbox_program: inbox,
            }))
            .unwrap(),
        }
    }

    fn good_ix(&self) -> Instruction {
        self.ix(
            &self.upgrade_authority.pubkey(),
            solana_loader_v3_interface::get_program_data_address(&bridge_program_id()),
            settlement_program_id(),
            inbox_program_id(),
        )
    }

    async fn send(
        &self,
        ctx: &mut ProgramTestContext,
        ix: Instruction,
        signer: &Keypair,
    ) -> Result<(), TransactionError> {
        let (r, cu, _) =
            rome_zk_testkit::send_measuring_cu(ctx, &[ix], &self.payer, &[signer]).await;
        eprintln!("InitBridgeConfig consumed {cu} CU");
        r
    }
}

#[tokio::test]
async fn init_bridge_config_writes_the_config_once_under_the_upgrade_authority() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    rig.send(&mut ctx, rig.good_ix(), &rig.upgrade_authority)
        .await
        .expect("InitBridgeConfig must succeed under the upgrade authority");
    let a = get_account(&mut ctx, bridge_config::pda(&bridge_program_id()).0)
        .await
        .expect("config must exist");
    assert_eq!(a.owner, bridge_program_id());
    assert_eq!(a.data.len(), bridge_config::LEN);
    let f = bridge_config::read(&a.data).unwrap();
    assert_eq!(f.settlement_program, settlement_program_id().to_bytes());
    assert_eq!(f.inbox_program, inbox_program_id().to_bytes());
}

#[tokio::test]
async fn init_bridge_config_refuses_a_signer_that_is_not_the_upgrade_authority() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    let impostor = Keypair::new();
    ctx.set_account(&impostor.pubkey(), &funded_account(1_000_000_000).into());
    let ix = rig.ix(
        &impostor.pubkey(),
        solana_loader_v3_interface::get_program_data_address(&bridge_program_id()),
        settlement_program_id(),
        inbox_program_id(),
    );
    let r = rig.send(&mut ctx, ix, &impostor).await;
    assert_custom(r, BridgeError::NotUpgradeAuthority);
    assert!(
        get_account(&mut ctx, bridge_config::pda(&bridge_program_id()).0)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn init_bridge_config_refuses_a_program_data_account_at_another_address() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    // A forged ProgramData-shaped account naming the signer as authority, at an address that is not this
    // program's ProgramData address, owned by the real loader.
    let fake = Pubkey::new_unique();
    let real = solana_loader_v3_interface::get_program_data_address(&bridge_program_id());
    let mut a = ctx.banks_client.get_account(real).await.unwrap().unwrap();
    a.data[13..45].copy_from_slice(rig.upgrade_authority.pubkey().as_ref());
    ctx.set_account(&fake, &a.into());
    let ix = rig.ix(
        &rig.upgrade_authority.pubkey(),
        fake,
        settlement_program_id(),
        inbox_program_id(),
    );
    let r = rig.send(&mut ctx, ix, &rig.upgrade_authority).await;
    assert_custom(r, BridgeError::NotUpgradeAuthority);
}

#[tokio::test]
async fn init_bridge_config_refuses_a_program_data_account_not_owned_by_the_loader() {
    // The bridge is loaded as a plain (non-upgradeable) program here, so a hand-made account sits at the
    // derived ProgramData address with the right contents and the right authority, owned by the system
    // program. Only the owner check can refuse it.
    let rig = CfgRig::new();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new(
            "zk_bridge",
            bridge_program_id(),
        )],
        true,
    );
    pt.add_account(rig.payer.pubkey(), funded_account(50_000_000_000));
    pt.add_account(
        rig.upgrade_authority.pubkey(),
        funded_account(1_000_000_000),
    );
    let pd = solana_loader_v3_interface::get_program_data_address(&bridge_program_id());
    let mut data = vec![0u8; 45];
    data[0..4].copy_from_slice(&3u32.to_le_bytes());
    data[12] = 1;
    data[13..45].copy_from_slice(rig.upgrade_authority.pubkey().as_ref());
    pt.add_account(
        pd,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;
    let r = rig
        .send(&mut ctx, rig.good_ix(), &rig.upgrade_authority)
        .await;
    assert_custom(r, BridgeError::NotUpgradeAuthority);
}

#[tokio::test]
async fn init_bridge_config_refuses_an_immutable_program() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    let real = solana_loader_v3_interface::get_program_data_address(&bridge_program_id());
    let mut a = ctx.banks_client.get_account(real).await.unwrap().unwrap();
    a.data[12] = 0; // upgrade_authority_address: None
    ctx.set_account(&real, &a.into());
    let r = rig
        .send(&mut ctx, rig.good_ix(), &rig.upgrade_authority)
        .await;
    assert_custom(r, BridgeError::NotUpgradeAuthority);
}

#[tokio::test]
async fn init_bridge_config_refuses_a_second_write() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    rig.send(&mut ctx, rig.good_ix(), &rig.upgrade_authority)
        .await
        .unwrap();
    // Different programs the second time: the first values must stay.
    let ix = rig.ix(
        &rig.upgrade_authority.pubkey(),
        solana_loader_v3_interface::get_program_data_address(&bridge_program_id()),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let r = rig.send(&mut ctx, ix, &rig.upgrade_authority).await;
    assert_custom(r, BridgeError::BridgeConfigAlreadyInitialized);
    let a = get_account(&mut ctx, bridge_config::pda(&bridge_program_id()).0)
        .await
        .unwrap();
    let f = bridge_config::read(&a.data).unwrap();
    assert_eq!(f.settlement_program, settlement_program_id().to_bytes());
}

#[tokio::test]
async fn init_bridge_config_refuses_a_zero_settlement_program() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    let ix = rig.ix(
        &rig.upgrade_authority.pubkey(),
        solana_loader_v3_interface::get_program_data_address(&bridge_program_id()),
        Pubkey::default(),
        inbox_program_id(),
    );
    let r = rig.send(&mut ctx, ix, &rig.upgrade_authority).await;
    assert_custom(r, BridgeError::BridgeConfigProgramZero);
}

#[tokio::test]
async fn init_bridge_config_refuses_a_zero_inbox_program() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(false).await;
    let ix = rig.ix(
        &rig.upgrade_authority.pubkey(),
        solana_loader_v3_interface::get_program_data_address(&bridge_program_id()),
        settlement_program_id(),
        Pubkey::default(),
    );
    let r = rig.send(&mut ctx, ix, &rig.upgrade_authority).await;
    assert_custom(r, BridgeError::BridgeConfigProgramZero);
}

#[tokio::test]
async fn init_bridge_config_adopts_a_prefunded_config_pda() {
    let rig = CfgRig::new();
    let mut ctx = rig.start(true).await;
    rig.send(&mut ctx, rig.good_ix(), &rig.upgrade_authority)
        .await
        .expect("a pre-funded config PDA must be adopted, not refused");
    let a = get_account(&mut ctx, bridge_config::pda(&bridge_program_id()).0)
        .await
        .unwrap();
    assert_eq!(a.owner, bridge_program_id());
    assert_eq!(a.data.len(), bridge_config::LEN);
    assert_eq!(
        bridge_config::read(&a.data).unwrap().inbox_program,
        inbox_program_id().to_bytes()
    );
}

// ------------------------------------------------------------------------------------------------
// InitDepositQueue
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn init_deposit_queue_creates_an_empty_queue_at_the_seed_hash() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    rig.send(&mut ctx, rig.init_ix(), "InitDepositQueue")
        .await
        .expect("InitDepositQueue must succeed");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.count, 0);
    assert_eq!(
        f.head_hash,
        rome_zk_layouts::deposit::queue_seed_hash(
            &soft_keccak,
            &settlement_program_id().to_bytes(),
            CHAIN
        ),
        "head_hash must be h_0"
    );
    assert_eq!(f.params, to_layout(&default_params()));
    assert_eq!(f.pending, DepositParams::default());
    assert_eq!(f.activation_slot, 0);
}

/// A chain that has never posted can be reclaimed by anyone, so it may not hold a queue: refused by name, with
/// no queue created.
#[tokio::test]
async fn init_deposit_queue_refuses_a_chain_that_never_posted() {
    let mut rig = Rig::new();
    rig.root.1 = never_posted_root(
        CHAIN,
        &rig.authority.pubkey(),
        settlement_program_id(),
        CHALLENGE_WINDOW,
    );
    rig.init_refused(BridgeError::ChainNeverPosted).await;
}

/// The first posted batch is enough.
#[tokio::test]
async fn init_deposit_queue_accepts_a_chain_that_posted_one_batch() {
    let mut rig = Rig::new();
    let mut a = never_posted_root(
        CHAIN,
        &rig.authority.pubkey(),
        settlement_program_id(),
        CHALLENGE_WINDOW,
    );
    let o = rome_zk_layouts::root::OFF_HEAD_PENDING_BATCH;
    a.data[o..o + 8].copy_from_slice(&1u64.to_le_bytes());
    rig.root.1 = a;
    let mut ctx = rig.start().await;
    rig.send(
        &mut ctx,
        rig.init_ix(),
        "InitDepositQueue (one batch posted)",
    )
    .await
    .expect("a chain with one posted batch may hold a queue");
}

#[tokio::test]
async fn init_deposit_queue_accepts_the_bounds_exactly() {
    let mut rig = Rig::new();
    rig.params = DepositParamsArgs {
        inclusion_deadline_secs: 3_600,
        max_per_batch: 256,
        max_per_block: 256,
        min_amount: 1,
        fee_lamports: 10_000_000,
        fee_recipient: fee_recipient_key(),
    };
    let mut ctx = rig.start().await;
    rig.send(
        &mut ctx,
        rig.init_ix(),
        "InitDepositQueue (lower and upper bounds)",
    )
    .await
    .expect("the bounds themselves must be accepted");
    let mut rig2 = Rig::new();
    rig2.params.inclusion_deadline_secs = 86_400;
    rig2.params.max_per_block = 1;
    let mut ctx2 = rig2.start().await;
    rig2.send(
        &mut ctx2,
        rig2.init_ix(),
        "InitDepositQueue (24 h, block cap 1)",
    )
    .await
    .expect("a 24 hour deadline and a block cap of 1 must be accepted");
}

#[tokio::test]
async fn init_deposit_queue_accepts_a_mint_of_exactly_nine_decimals() {
    let mut rig = Rig::new();
    rig.vault = Some((
        rig.vault.as_ref().unwrap().0,
        vault_account(CHAIN, 9, bridge_program_id()),
    ));
    let mut ctx = rig.start().await;
    rig.send(&mut ctx, rig.init_ix(), "InitDepositQueue (9 decimals)")
        .await
        .unwrap();
}

#[tokio::test]
async fn init_deposit_queue_adopts_a_prefunded_queue_pda() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    rome_zk_testkit::prefund_pda(&mut ctx, rig.queue.0).await;
    rig.send(&mut ctx, rig.init_ix(), "InitDepositQueue (pre-funded PDA)")
        .await
        .expect("a pre-funded queue PDA must be adopted, not refused");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.count, 0);
    assert_eq!(f.params, to_layout(&default_params()));
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_second_queue_for_the_chain() {
    let mut rig = Rig::new();
    rig.queue.1 = Some(queue_account(bridge_program_id(), &default_params(), None));
    rig.init_refused(BridgeError::QueueAlreadyInitialized).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_missing_bridge_config() {
    let mut rig = Rig::new();
    rig.config = Some((
        bridge_config::pda(&bridge_program_id()).0,
        funded_account(0),
    ));
    rig.init_refused(BridgeError::WrongBridgeConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_bridge_config_owned_by_another_program() {
    let mut rig = Rig::new();
    rig.config = Some((
        bridge_config::pda(&bridge_program_id()).0,
        bridge_config_account(
            Pubkey::new_unique(),
            settlement_program_id(),
            inbox_program_id(),
        ),
    ));
    rig.init_refused(BridgeError::WrongBridgeConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_bridge_config_at_another_address() {
    let mut rig = Rig::new();
    rig.config = Some((
        Pubkey::new_unique(),
        bridge_config_account(
            bridge_program_id(),
            settlement_program_id(),
            inbox_program_id(),
        ),
    ));
    rig.init_refused(BridgeError::WrongBridgeConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_settlement_program_other_than_the_configs() {
    let mut rig = Rig::new();
    rig.settlement_arg = Pubkey::new_unique();
    rig.init_refused(BridgeError::WrongSettlementProgram).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_reserved_chain_id() {
    // Every account is valid for the reserved id 200101, so only the reserved-id rule can refuse.
    let bridge = bridge_program_id();
    let settlement = settlement_program_id();
    let mut rig = Rig::new();
    let id = common::CHAIN_ID;
    rig.chain_id = id;
    rig.root = (
        rome_zk_layouts::root::pda(&settlement, id).0,
        root_account(id, &rig.authority.pubkey(), settlement, CHALLENGE_WINDOW),
    );
    rig.registry = (
        rome_zk_layouts::registry::pda(&settlement, id).0,
        registry_account(id, &inbox_program_id(), settlement),
    );
    rig.vault = Some((
        zk_bridge::state::vault_config_pda(&bridge, &settlement, id).0,
        vault_account(id, 9, bridge),
    ));
    rig.queue.0 = queue_layout::pda(&bridge, &settlement.to_bytes(), id).0;
    rig.init_refused(BridgeError::ReservedChainId).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_the_highest_reserved_chain_id() {
    let bridge = bridge_program_id();
    let settlement = settlement_program_id();
    let mut rig = Rig::new();
    let id = (1u64 << 32) - 1;
    rig.chain_id = id;
    rig.root = (
        rome_zk_layouts::root::pda(&settlement, id).0,
        root_account(id, &rig.authority.pubkey(), settlement, CHALLENGE_WINDOW),
    );
    rig.registry = (
        rome_zk_layouts::registry::pda(&settlement, id).0,
        registry_account(id, &inbox_program_id(), settlement),
    );
    rig.vault = Some((
        zk_bridge::state::vault_config_pda(&bridge, &settlement, id).0,
        vault_account(id, 9, bridge),
    ));
    rig.queue.0 = queue_layout::pda(&bridge, &settlement.to_bytes(), id).0;
    rig.init_refused(BridgeError::ReservedChainId).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_root_at_another_address() {
    let mut rig = Rig::new();
    rig.root.0 = Pubkey::new_unique();
    rig.init_refused(BridgeError::RootNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_root_owned_by_another_program() {
    let mut rig = Rig::new();
    rig.root.1 = root_account(
        CHAIN,
        &rig.authority.pubkey(),
        Pubkey::new_unique(),
        CHALLENGE_WINDOW,
    );
    rig.init_refused(BridgeError::RootNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_root_under_another_settlement_program() {
    // A hostile settlement program's own root, at its own PDA, naming the signer as authority.
    let hostile = Pubkey::new_unique();
    let mut rig = Rig::new();
    rig.root = (
        rome_zk_layouts::root::pda(&hostile, CHAIN).0,
        root_account(CHAIN, &rig.authority.pubkey(), hostile, CHALLENGE_WINDOW),
    );
    rig.init_refused(BridgeError::RootNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_signer_that_is_not_the_chain_authority() {
    let mut rig = Rig::new();
    rig.signer = Keypair::new();
    rig.init_refused(BridgeError::NotChainAuthority).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_registry_at_another_address() {
    let mut rig = Rig::new();
    rig.registry.0 = Pubkey::new_unique();
    rig.init_refused(BridgeError::RegistryNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_registry_owned_by_another_program() {
    let mut rig = Rig::new();
    rig.registry.1 = registry_account(CHAIN, &inbox_program_id(), Pubkey::new_unique());
    rig.init_refused(BridgeError::RegistryNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_registry_for_another_chain() {
    let mut rig = Rig::new();
    rig.registry.1 = registry_account(CHAIN + 1, &inbox_program_id(), settlement_program_id());
    rig.init_refused(BridgeError::RegistryNotCanonical).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_registry_naming_another_inbox() {
    let mut rig = Rig::new();
    rig.registry.1 = registry_account(CHAIN, &Pubkey::new_unique(), settlement_program_id());
    rig.init_refused(BridgeError::WrongInboxProgram).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_missing_vault_config() {
    let mut rig = Rig::new();
    rig.vault = Some((rig.vault.as_ref().unwrap().0, funded_account(0)));
    rig.init_refused(BridgeError::WrongVaultConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_vault_config_at_another_address() {
    let mut rig = Rig::new();
    rig.vault = Some((
        Pubkey::new_unique(),
        vault_account(CHAIN, 9, bridge_program_id()),
    ));
    rig.init_refused(BridgeError::WrongVaultConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_vault_config_owned_by_another_program() {
    let mut rig = Rig::new();
    rig.vault = Some((
        rig.vault.as_ref().unwrap().0,
        vault_account(CHAIN, 9, Pubkey::new_unique()),
    ));
    rig.init_refused(BridgeError::WrongVaultConfig).await;
}

#[tokio::test]
async fn init_deposit_queue_refuses_a_mint_of_ten_decimals() {
    let mut rig = Rig::new();
    rig.vault = Some((
        rig.vault.as_ref().unwrap().0,
        vault_account(CHAIN, 10, bridge_program_id()),
    ));
    rig.init_refused(BridgeError::MintTooManyDecimals).await;
}

/// One test per parameter bound, for both `InitDepositQueue` and `ProposeDepositParams`: the same bound
/// must refuse at both.
macro_rules! bound_tests {
    ($init:ident, $propose:ident, $mutate:expr, $err:expr) => {
        #[tokio::test]
        async fn $init() {
            let mut rig = Rig::new();
            let m: fn(&mut Rig) = $mutate;
            m(&mut rig);
            rig.init_refused($err).await;
        }
        #[tokio::test]
        async fn $propose() {
            let mut rig = Rig::with_queue();
            let m: fn(&mut Rig) = $mutate;
            m(&mut rig);
            rig.propose_refused(100 + CHALLENGE_WINDOW as u64 + 10, $err)
                .await;
        }
    };
}

bound_tests!(
    init_deposit_queue_refuses_a_deadline_under_one_hour,
    propose_deposit_params_refuses_a_deadline_under_one_hour,
    |r| r.params.inclusion_deadline_secs = 3_599,
    BridgeError::DeadlineBelowFloor
);
bound_tests!(
    init_deposit_queue_refuses_a_deadline_over_24_hours,
    propose_deposit_params_refuses_a_deadline_over_24_hours,
    |r| r.params.inclusion_deadline_secs = 86_401,
    BridgeError::DeadlineAboveCeiling
);
bound_tests!(
    init_deposit_queue_refuses_257_per_batch,
    propose_deposit_params_refuses_257_per_batch,
    |r| r.params.max_per_batch = 257,
    BridgeError::MaxPerBatchTooLarge
);
bound_tests!(
    init_deposit_queue_refuses_zero_per_block,
    propose_deposit_params_refuses_zero_per_block,
    |r| r.params.max_per_block = 0,
    BridgeError::MaxPerBlockOutOfRange
);
bound_tests!(
    init_deposit_queue_refuses_a_block_cap_over_the_batch_cap,
    propose_deposit_params_refuses_a_block_cap_over_the_batch_cap,
    |r| {
        r.params.max_per_batch = 8;
        r.params.max_per_block = 9;
    },
    BridgeError::MaxPerBlockOutOfRange
);
bound_tests!(
    init_deposit_queue_refuses_a_zero_minimum_amount,
    propose_deposit_params_refuses_a_zero_minimum_amount,
    |r| r.params.min_amount = 0,
    BridgeError::MinAmountZero
);
bound_tests!(
    init_deposit_queue_refuses_a_fee_over_0_01_sol,
    propose_deposit_params_refuses_a_fee_over_0_01_sol,
    |r| r.params.fee_lamports = 10_000_001,
    BridgeError::FeeTooHigh
);
bound_tests!(
    init_deposit_queue_refuses_a_fee_recipient_under_rent,
    propose_deposit_params_refuses_a_fee_recipient_under_rent,
    |r| r.fee_recipient.1 = fee_recipient_account(rome_zk_testkit::rent_exempt(0) - 1),
    BridgeError::FeeRecipientNotRentExempt
);
bound_tests!(
    init_deposit_queue_refuses_a_fee_recipient_account_that_is_not_the_parameter,
    propose_deposit_params_refuses_a_fee_recipient_account_that_is_not_the_parameter,
    |r| {
        let other = Pubkey::new_unique();
        r.fee_recipient = (other, fee_recipient_account(1_000_000));
    },
    BridgeError::FeeRecipientNotRentExempt
);

bound_tests!(
    init_deposit_queue_refuses_an_executable_fee_recipient,
    propose_deposit_params_refuses_an_executable_fee_recipient,
    |r| {
        let mut a = fee_recipient_account(1_000_000);
        a.executable = true;
        a.owner = solana_sdk_ids::bpf_loader::id();
        r.fee_recipient.1 = a;
    },
    BridgeError::FeeRecipientNotPlain
);
bound_tests!(
    init_deposit_queue_refuses_a_sysvar_fee_recipient,
    propose_deposit_params_refuses_a_sysvar_fee_recipient,
    |r| {
        let mut a = fee_recipient_account(1_000_000);
        a.owner = solana_sdk_ids::sysvar::id();
        r.fee_recipient.1 = a;
    },
    BridgeError::FeeRecipientNotPlain
);

// ------------------------------------------------------------------------------------------------
// ProposeDepositParams
// ------------------------------------------------------------------------------------------------

fn new_params() -> DepositParamsArgs {
    DepositParamsArgs {
        inclusion_deadline_secs: 7_200,
        max_per_batch: 128,
        max_per_block: 16,
        min_amount: 5_000,
        fee_lamports: 2_000_000,
        fee_recipient: fee_recipient_key(),
    }
}

#[tokio::test]
async fn propose_deposit_params_records_a_pending_set_and_leaves_the_live_one() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let now = now_slot(&mut ctx).await;
    let activation = now + CHALLENGE_WINDOW as u64;
    rig.send(&mut ctx, rig.propose_ix(activation), "ProposeDepositParams")
        .await
        .expect("a proposal exactly one challenge window away must be accepted");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(
        f.params,
        to_layout(&default_params()),
        "live parameters must not move"
    );
    assert_eq!(f.pending, to_layout(&new_params()));
    assert_eq!(f.activation_slot, activation);
}

#[tokio::test]
async fn propose_deposit_params_refuses_an_activation_under_one_challenge_window() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    // Slot 100 plus the window is the earliest; one less is too soon.
    rig.propose_refused(
        100 + CHALLENGE_WINDOW as u64 - 1,
        BridgeError::ActivationTooSoon,
    )
    .await;
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_chain_with_a_zero_challenge_window() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.root.1 = root_account(CHAIN, &rig.authority.pubkey(), settlement_program_id(), 0);
    rig.propose_refused(1_000, BridgeError::ChallengeWindowZero)
        .await;
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_signer_that_is_not_the_chain_authority() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.signer = Keypair::new();
    rig.propose_refused(1_000, BridgeError::NotChainAuthority)
        .await;
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_root_at_another_address() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.root.0 = Pubkey::new_unique();
    rig.propose_refused(1_000, BridgeError::RootNotCanonical)
        .await;
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_root_owned_by_another_program() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.root.1 = root_account(
        CHAIN,
        &rig.authority.pubkey(),
        Pubkey::new_unique(),
        CHALLENGE_WINDOW,
    );
    rig.propose_refused(1_000, BridgeError::RootNotCanonical)
        .await;
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_queue_at_another_address() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    // A bridge-owned queue-shaped account that is not at the chain's queue address.
    let k = Pubkey::new_unique();
    let acc = rig.queue.1.take().unwrap();
    let mut ctx_rig = rig;
    ctx_rig.queue.0 = k;
    ctx_rig.queue.1 = Some(acc);
    let mut ctx = ctx_rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let r = ctx_rig
        .send(
            &mut ctx,
            ctx_rig.propose_ix(1_000),
            "ProposeDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongDepositQueue);
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_queue_owned_by_another_program() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.queue.1 = Some(queue_account(Pubkey::new_unique(), &default_params(), None));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.propose_ix(1_000),
            "ProposeDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongDepositQueue);
}

#[tokio::test]
async fn propose_deposit_params_refuses_a_missing_bridge_config() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    rig.config = Some((
        bridge_config::pda(&bridge_program_id()).0,
        funded_account(0),
    ));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.propose_ix(1_000),
            "ProposeDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongBridgeConfig);
}

/// A pending proposal that can never activate (an activation slot out of reach) is replaced by a new valid one.
#[tokio::test]
async fn propose_deposit_params_replaces_a_stuck_proposal() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    // The shape an earlier build let through: a proposal with no upper bound on its activation slot.
    rig.set_queue(Some((&default_params(), u64::MAX)));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let activation = 100 + CHALLENGE_WINDOW as u64;
    rig.send(
        &mut ctx,
        rig.propose_ix(activation),
        "ProposeDepositParams (replace)",
    )
    .await
    .expect("a new valid proposal must replace the stuck one");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.pending, to_layout(&new_params()));
    assert_eq!(f.activation_slot, activation);
    assert_eq!(
        f.params,
        to_layout(&default_params()),
        "live parameters must not move"
    );

    // And the replacement activates.
    ctx.warp_to_slot(activation).unwrap();
    rig.send(&mut ctx, rig.activate_ix(), "ActivateDepositParams")
        .await
        .expect("the replacement must activate");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&new_params()));
    assert_eq!(f.activation_slot, 0);
}

/// A pending proposal whose fee recipient was sabotaged after the proposal (it can no longer activate) is
/// replaced by a proposal naming another recipient.
#[tokio::test]
async fn propose_deposit_params_replaces_a_proposal_whose_fee_recipient_was_sabotaged() {
    let mut rig = Rig::with_queue();
    let mut stuck = new_params();
    let sabotaged = Pubkey::new_from_array([0xab; 32]);
    stuck.fee_recipient = sabotaged;
    rig.set_queue(Some((&stuck, 200)));
    rig.params = new_params();
    let mut ctx = rig.start().await;
    // The holder of the proposed fee recipient's key hands the account to the sysvar owner.
    let mut a = fee_recipient_account(1_000_000);
    a.owner = solana_sdk_ids::sysvar::id();
    ctx.set_account(&sabotaged, &a.into());
    ctx.warp_to_slot(200).unwrap();
    // Activation of the stuck proposal fails.
    let mut ix = rig.activate_ix();
    ix.accounts[2] = AccountMeta::new_readonly(sabotaged, false);
    let r = rig
        .send(&mut ctx, ix, "ActivateDepositParams (sabotaged)")
        .await;
    assert_custom(r, BridgeError::FeeRecipientNotPlain);
    // A new proposal replaces it, and activates.
    let activation = 200 + CHALLENGE_WINDOW as u64;
    rig.send(
        &mut ctx,
        rig.propose_ix(activation),
        "ProposeDepositParams (replace)",
    )
    .await
    .expect("a new valid proposal must replace the sabotaged one");
    ctx.warp_to_slot(activation).unwrap();
    rig.send(&mut ctx, rig.activate_ix(), "ActivateDepositParams")
        .await
        .expect("the replacement must activate");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&new_params()));
}

/// A replacement still has to be valid, and a refused one leaves the pending proposal as it was.
#[tokio::test]
async fn propose_deposit_params_refuses_an_invalid_replacement_and_keeps_the_pending_one() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 500)));
    rig.params = new_params();
    rig.params.inclusion_deadline_secs = 3_599;
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.propose_ix(100 + CHALLENGE_WINDOW as u64),
            "ProposeDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::DeadlineBelowFloor);
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.pending, to_layout(&new_params()));
    assert_eq!(f.activation_slot, 500);
}

/// An activation slot is at most two challenge windows away: exactly two is accepted, one more is refused, and
/// so is the far-future value that used to freeze the queue's parameters.
#[tokio::test]
async fn propose_deposit_params_accepts_exactly_two_windows_and_refuses_beyond() {
    let mut rig = Rig::with_queue();
    rig.params = new_params();
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(100).unwrap();
    let two = 100 + 2 * CHALLENGE_WINDOW as u64;
    let r = rig
        .send(
            &mut ctx,
            rig.propose_ix(two + 1),
            "ProposeDepositParams (too late)",
        )
        .await;
    assert_custom(r, BridgeError::ActivationTooLate);
    let r = rig
        .send(
            &mut ctx,
            rig.propose_ix(u64::MAX),
            "ProposeDepositParams (u64::MAX)",
        )
        .await;
    assert_custom(r, BridgeError::ActivationTooLate);
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.activation_slot, 0, "a refused proposal records nothing");
    rig.send(
        &mut ctx,
        rig.propose_ix(two),
        "ProposeDepositParams (two windows)",
    )
    .await
    .expect("an activation exactly two windows away must be accepted");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.activation_slot, two);
}

// ------------------------------------------------------------------------------------------------
// ActivateDepositParams
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn activate_deposit_params_is_refused_before_its_slot_and_applied_after_it() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(199).unwrap();
    let r = rig
        .send(&mut ctx, rig.activate_ix(), "ActivateDepositParams (early)")
        .await;
    assert_custom(r, BridgeError::ActivationNotReached);
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(
        f.params,
        to_layout(&default_params()),
        "an early activation must change nothing"
    );
    assert_eq!(f.activation_slot, 200);

    ctx.warp_to_slot(200).unwrap();
    rig.send(&mut ctx, rig.activate_ix(), "ActivateDepositParams")
        .await
        .expect("activation at its slot must succeed");
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&new_params()));
    assert_eq!(f.pending, DepositParams::default());
    assert_eq!(f.activation_slot, 0);
    assert_eq!(f.count, 0, "activation must not touch the count");
    assert_eq!(
        f.head_hash, [0x99; 32],
        "activation must not touch the head hash"
    );
}

/// A proposal passed the fee-recipient check when it was made; the account can change before it activates.
async fn activation_refused_with_fee_recipient(account: Account, want: BridgeError) {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    let mut ctx = rig.start().await;
    ctx.set_account(&rig.fee_recipient.0, &account.into());
    ctx.warp_to_slot(200).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, want);
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&default_params()));
    assert_eq!(
        f.activation_slot, 200,
        "a refused activation changes nothing"
    );
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_fee_recipient_that_fell_under_rent() {
    activation_refused_with_fee_recipient(
        fee_recipient_account(rome_zk_testkit::rent_exempt(0) - 1),
        BridgeError::FeeRecipientNotRentExempt,
    )
    .await;
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_fee_recipient_that_became_executable() {
    let mut a = fee_recipient_account(1_000_000);
    a.executable = true;
    a.owner = solana_sdk_ids::bpf_loader::id();
    activation_refused_with_fee_recipient(a, BridgeError::FeeRecipientNotPlain).await;
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_fee_recipient_account_that_is_not_the_proposed_one() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    let mut ctx = rig.start().await;
    let other = Pubkey::new_unique();
    ctx.set_account(&other, &fee_recipient_account(1_000_000).into());
    ctx.warp_to_slot(200).unwrap();
    let mut ix = rig.activate_ix();
    ix.accounts[2] = AccountMeta::new_readonly(other, false);
    let r = rig
        .send(&mut ctx, ix, "ActivateDepositParams (refused)")
        .await;
    assert_custom(r, BridgeError::FeeRecipientNotRentExempt);
}

/// The bounds are checked again at activation, on the pending values themselves: a proposal that is out of
/// bounds when it activates (for instance after an upgrade tightened them) is refused and changes nothing.
async fn activation_refused_with_pending(pending: DepositParamsArgs, want: BridgeError) {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&pending, 200)));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(200).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, want);
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&default_params()));
    assert_eq!(f.pending, to_layout(&pending));
    assert_eq!(f.activation_slot, 200);
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_pending_deadline_out_of_bounds() {
    let mut p = new_params();
    p.inclusion_deadline_secs = 3_599;
    activation_refused_with_pending(p, BridgeError::DeadlineBelowFloor).await;
    let mut p = new_params();
    p.inclusion_deadline_secs = 86_401;
    activation_refused_with_pending(p, BridgeError::DeadlineAboveCeiling).await;
}

#[tokio::test]
async fn activate_deposit_params_refuses_pending_caps_out_of_bounds() {
    let mut p = new_params();
    p.max_per_batch = 257;
    activation_refused_with_pending(p, BridgeError::MaxPerBatchTooLarge).await;
    let mut p = new_params();
    p.max_per_block = 0;
    activation_refused_with_pending(p, BridgeError::MaxPerBlockOutOfRange).await;
    let mut p = new_params();
    p.max_per_batch = 8;
    p.max_per_block = 9;
    activation_refused_with_pending(p, BridgeError::MaxPerBlockOutOfRange).await;
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_pending_minimum_or_fee_out_of_bounds() {
    let mut p = new_params();
    p.min_amount = 0;
    activation_refused_with_pending(p, BridgeError::MinAmountZero).await;
    let mut p = new_params();
    p.fee_lamports = 10_000_001;
    activation_refused_with_pending(p, BridgeError::FeeTooHigh).await;
}

#[tokio::test]
async fn activate_deposit_params_refuses_when_nothing_is_pending() {
    let rig = Rig::with_queue();
    let mut ctx = rig.start().await;
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::NoPendingParams);
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_second_activation() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(200).unwrap();
    rig.send(&mut ctx, rig.activate_ix(), "ActivateDepositParams")
        .await
        .unwrap();
    let r = rig
        .send(&mut ctx, rig.activate_ix(), "ActivateDepositParams (again)")
        .await;
    assert_custom(r, BridgeError::NoPendingParams);
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_queue_at_another_address() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    let acc = rig.queue.1.clone();
    rig.queue.0 = Pubkey::new_unique();
    rig.queue.1 = acc;
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(200).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongDepositQueue);
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_queue_owned_by_another_program() {
    let mut rig = Rig::with_queue();
    rig.queue.1 = Some(queue_account(
        Pubkey::new_unique(),
        &default_params(),
        Some((&new_params(), 200)),
    ));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(200).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongDepositQueue);
}

#[tokio::test]
async fn activate_deposit_params_refuses_a_missing_bridge_config() {
    let mut rig = Rig::with_queue();
    rig.set_queue(Some((&new_params(), 200)));
    rig.config = Some((
        bridge_config::pda(&bridge_program_id()).0,
        funded_account(0),
    ));
    let mut ctx = rig.start().await;
    ctx.warp_to_slot(200).unwrap();
    let r = rig
        .send(
            &mut ctx,
            rig.activate_ix(),
            "ActivateDepositParams (refused)",
        )
        .await;
    assert_custom(r, BridgeError::WrongBridgeConfig);
}

// ------------------------------------------------------------------------------------------------
// The whole path, and the held tag
// ------------------------------------------------------------------------------------------------

/// Config, queue, proposal and activation in one run, each step through the real program with no fixture
/// for the bridge's own accounts.
#[tokio::test]
async fn config_queue_propose_and_activate_work_end_to_end() {
    let mut rig = Rig::new();
    rig.config = None;
    let cfg = CfgRig {
        payer: rig.payer.insecure_clone(),
        upgrade_authority: Keypair::new(),
    };
    // Start from the config rig's context (upgradeable bridge), then add the settlement fixtures.
    let mut ctx = cfg.start(false).await;
    ctx.set_account(&rig.signer.pubkey(), &funded_account(1_000_000_000).into());
    for (k, a) in [&rig.root, &rig.registry, &rig.fee_recipient] {
        ctx.set_account(k, &a.clone().into());
    }
    let v = rig.vault.as_ref().unwrap();
    ctx.set_account(&v.0, &v.1.clone().into());

    cfg.send(&mut ctx, cfg.good_ix(), &cfg.upgrade_authority)
        .await
        .unwrap();
    rig.config = Some((
        bridge_config::pda(&bridge_program_id()).0,
        Account::default(),
    ));
    rig.send(&mut ctx, rig.init_ix(), "InitDepositQueue")
        .await
        .unwrap();
    ctx.warp_to_slot(100).unwrap();
    rig.params = new_params();
    rig.send(
        &mut ctx,
        rig.propose_ix(100 + CHALLENGE_WINDOW as u64),
        "ProposeDepositParams",
    )
    .await
    .unwrap();
    ctx.warp_to_slot(100 + CHALLENGE_WINDOW as u64).unwrap();
    rig.send(&mut ctx, rig.activate_ix(), "ActivateDepositParams")
        .await
        .unwrap();
    let f = read_queue(&mut ctx, rig.queue.0).await;
    assert_eq!(f.params, to_layout(&new_params()));
}
