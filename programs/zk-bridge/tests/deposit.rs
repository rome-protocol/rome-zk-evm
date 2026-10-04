//! Real-BPF tests for `Deposit` and `CloseDeposit`. The `.so` files are the `cargo build-sbf` output (build
//! them first with `cargo build-sbf --arch v3` for each program). The chain's settlement accounts (root,
//! registry, exit config), the bridge config, the vault and the queue are fixtures built as accounts; the
//! instruction under test runs as the real program, and the tokens move through the real SPL Token program.
//!
//! The golden test signs three deposits as the synthetic small batch's seed depositors and checks the
//! records and the hash chain's end against `fixtures/prover-input/synthetic-deposits-small.json`.
//!
//! Every instruction prints the compute units it used.

mod common;

use common::{
    bridge_program_id, decode_token_amount, exit_config_account, funded_account, get_account,
    mint_account, settlement_program_id, token_account, vault_config_account, CHAIN_ID,
};
use rome_zk_layouts::deposit::{self, DepositRecord};
use rome_zk_layouts::deposit_queue::{
    bridge_config, deposit_queue as queue_layout, deposit_queue::DepositParams, deposit_record,
};
use solana_program::{
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
use std::collections::BTreeMap;
use zk_bridge::errors::BridgeError;

const MINT_DECIMALS: u8 = 9;
const MIN_AMOUNT: u64 = 1_000;
const FEE_LAMPORTS: u64 = 2_000_000;
const DEPOSITOR_TOKENS: u64 = 10_000_000_000;
const DEPOSITOR_LAMPORTS: u64 = 10_000_000_000;

// Positions of the accounts in a `Deposit` instruction.
const D_VAULT_CONFIG: usize = 2;
const D_VAULT_TOKEN: usize = 3;
const D_QUEUE: usize = 4;
const D_EXIT_CONFIG: usize = 6;
const D_FEE_RECIPIENT: usize = 7;
const D_ROOT: usize = 10;

// Positions of the accounts in a `CloseDeposit` instruction.
const C_REGISTRY: usize = 1;
const C_CURSOR: usize = 2;
const C_RECORD: usize = 3;
const C_RENT_RECIPIENT: usize = 4;

fn soft_keccak(parts: &[&[u8]]) -> [u8; 32] {
    keccak::hashv(parts).to_bytes()
}

fn inbox_program_id() -> Pubkey {
    rome_zk_testkit::fixed_inbox_program_id()
}
fn fee_recipient_key() -> Pubkey {
    Pubkey::new_from_array([0xfe; 32])
}
fn mint() -> Pubkey {
    rome_zk_testkit::fixed_mint_pubkey()
}
fn token_key(i: u64) -> Pubkey {
    Pubkey::new_from_array([0xA0 + i as u8; 32])
}
/// A settlement program that is not the canonical one.
fn other_settlement() -> Pubkey {
    Pubkey::new_from_array([0x52; 32])
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

fn assert_custom(result: Result<(), TransactionError>, want: BridgeError) {
    match result {
        Err(TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::Custom(c),
        )) => assert_eq!(c, want as u32, "expected {want:?}, got Custom({c})"),
        other => panic!("expected {want:?}, got {other:?}"),
    }
}

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    assert_eq!(s.len(), 64, "a 32-byte hex string");
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The value of a top-level `"key": "<64 hex>"` pair in the synthetic batch's sidecar, found by text so the
/// test needs no JSON dependency.
fn sidecar_hash(key: &str) -> [u8; 32] {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/prover-input/synthetic-deposits-small.json"
    );
    let text = std::fs::read_to_string(path).expect("the synthetic small batch's sidecar");
    let pat = format!("\"{key}\": \"");
    let at = text
        .find(&pat)
        .unwrap_or_else(|| panic!("{key} in the sidecar"))
        + pat.len();
    hex32(&text[at..at + 64])
}

/// Deposit `index` of the synthetic small batch: the sender is the seed depositor's wallet, the recipient and
/// amount come from the same fixed functions the batch generator uses.
fn synthetic_record(index: u64) -> DepositRecord {
    let r = soft_keccak(&[b"synthetic-deposits/v1/recipient", &index.to_le_bytes()]);
    let mut recipient = [0u8; 20];
    recipient.copy_from_slice(&r[12..]);
    DepositRecord {
        sender: rome_zk_testkit::synthetic_depositor_pubkey(index),
        recipient,
        amount_gwei: 1_000_000 + 12_345 * index,
    }
}

fn params() -> DepositParams {
    DepositParams {
        inclusion_deadline_secs: 43_200,
        max_per_batch: 64,
        max_per_block: 8,
        min_amount: MIN_AMOUNT,
        fee_lamports: FEE_LAMPORTS,
        fee_recipient: fee_recipient_key().to_bytes(),
    }
}

fn queue_account(settlement: &Pubkey, chain_id: u64, count: u64, head: [u8; 32]) -> Account {
    let _ = (settlement, chain_id);
    let mut d = vec![0u8; queue_layout::LEN];
    queue_layout::write(
        &mut d,
        &queue_layout::DepositQueueFields {
            count,
            head_hash: head,
            params: params(),
            pending: DepositParams::default(),
            activation_slot: 0,
        },
    );
    owned(bridge_program_id(), d)
}

fn registry_account(chain_id: u64, inbox: &Pubkey, owner: Pubkey) -> Account {
    use rome_zk_layouts::registry as r;
    let mut d = vec![0u8; r::REGISTRY_LEN];
    d[r::OFF_MAGIC..r::OFF_MAGIC + 4].copy_from_slice(&r::MAGIC.to_le_bytes());
    d[r::OFF_CHAIN_ID..r::OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
    d[r::OFF_INBOX_PROGRAM..r::OFF_INBOX_PROGRAM + 32].copy_from_slice(inbox.as_ref());
    owned(owner, d)
}

fn config_account(settlement: Pubkey, inbox: Pubkey) -> Account {
    let mut d = vec![0u8; bridge_config::LEN];
    bridge_config::write(
        &mut d,
        &bridge_config::BridgeConfigFields {
            settlement_program: settlement.to_bytes(),
            inbox_program: inbox.to_bytes(),
        },
    );
    owned(bridge_program_id(), d)
}

/// A version-2 cursor at the inbox's address for the chain: `next` deposits credited, `final_` of them final.
fn cursor_v2(owner: Pubkey, chain_id: u64, next: u64, final_: u64) -> Account {
    use rome_zk_layouts::cursor as c;
    let mut a = rome_zk_testkit::cursor_account_for(2, owner, chain_id, 1);
    a.data[c::OFF_DEPOSIT_NEXT..c::OFF_DEPOSIT_NEXT + 8].copy_from_slice(&next.to_le_bytes());
    a.data[c::OFF_DEPOSIT_FINAL..c::OFF_DEPOSIT_FINAL + 8].copy_from_slice(&final_.to_le_bytes());
    a
}

fn record_account(index: u64, r: &DepositRecord, hash_after: [u8; 32]) -> Account {
    let mut d = vec![0u8; deposit_record::LEN];
    deposit_record::write(
        &mut d,
        &deposit_record::DepositRecordFields {
            index,
            enqueue_unix_ts: 1,
            sender: r.sender,
            recipient: r.recipient,
            amount_gwei: r.amount_gwei,
            hash_after,
        },
    );
    owned(bridge_program_id(), d)
}

fn seed_hash(settlement: &Pubkey, chain_id: u64) -> [u8; 32] {
    deposit::queue_seed_hash(&soft_keccak, &settlement.to_bytes(), chain_id)
}

/// Every account a test starts with, by address. A test changes the entries its refusal is about and leaves
/// the rest valid.
struct Rig {
    payer: Keypair,
    accounts: BTreeMap<Pubkey, Account>,
    /// The synthetic depositors, by index.
    depositors: Vec<Keypair>,
}

impl Rig {
    /// Chain `CHAIN_ID` fully set up under the canonical settlement program: bridge config, vault, queue (empty,
    /// at the seed hash), root, registry, exit config, fee recipient, and three funded depositors with a token
    /// account each.
    fn new() -> Self {
        let bridge = bridge_program_id();
        let sp = settlement_program_id();
        let mut r = Rig {
            payer: Keypair::new(),
            accounts: BTreeMap::new(),
            depositors: (0..3)
                .map(rome_zk_testkit::synthetic_depositor_keypair)
                .collect(),
        };
        r.put(
            bridge_config::pda(&bridge).0,
            config_account(sp, inbox_program_id()),
        );
        r.put_chain(&sp, CHAIN_ID);
        r.put(
            fee_recipient_key(),
            funded_account(rome_zk_testkit::rent_exempt(0)),
        );
        for i in 0..3u64 {
            let w = r.depositors[i as usize].pubkey();
            r.put(w, funded_account(DEPOSITOR_LAMPORTS));
            r.put(token_key(i), token_account(&mint(), &w, DEPOSITOR_TOKENS));
        }
        r
    }

    /// The accounts of one chain under `sp`: vault config and token, queue, root, registry, exit config.
    fn put_chain(&mut self, sp: &Pubkey, chain_id: u64) {
        let bridge = bridge_program_id();
        let authority = Pubkey::new_unique();
        let (vault_authority, _) = zk_bridge::state::vault_authority_pda(&bridge, sp, chain_id);
        let mut vault = vault_config_account(*sp, mint(), MINT_DECIMALS, authority);
        let mut d = vault.data.clone();
        d[zk_bridge::state::vault_config::OFF_CHAIN_ID
            ..zk_bridge::state::vault_config::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        vault.data = d;
        self.put(
            zk_bridge::state::vault_config_pda(&bridge, sp, chain_id).0,
            vault,
        );
        self.put(
            zk_bridge::state::vault_token_pda(&bridge, sp, chain_id, &mint()).0,
            token_account(&mint(), &vault_authority, 0),
        );
        self.put(
            queue_layout::pda(&bridge, &sp.to_bytes(), chain_id).0,
            queue_account(sp, chain_id, 0, seed_hash(sp, chain_id)),
        );
        self.put(
            rome_zk_layouts::root::pda(sp, chain_id).0,
            rome_zk_testkit::root_account_with_authority(chain_id, &authority, *sp),
        );
        self.put(
            rome_zk_layouts::registry::pda(sp, chain_id).0,
            registry_account(chain_id, &inbox_program_id(), *sp),
        );
        let mut exit = exit_config_account(*sp, bridge);
        let o = rome_zk_layouts::exit::exit_config::OFF_CHAIN_ID;
        exit.data[o..o + 8].copy_from_slice(&chain_id.to_le_bytes());
        self.put(
            rome_zk_layouts::exit::exit_config::pda(sp, chain_id).0,
            exit,
        );
    }

    fn put(&mut self, key: Pubkey, a: Account) {
        self.accounts.insert(key, a);
    }

    fn set_queue_state(&mut self, count: u64, head: [u8; 32]) {
        let key = self.queue_key();
        self.put(
            key,
            queue_account(&settlement_program_id(), CHAIN_ID, count, head),
        );
    }

    fn queue_key(&self) -> Pubkey {
        queue_layout::pda(
            &bridge_program_id(),
            &settlement_program_id().to_bytes(),
            CHAIN_ID,
        )
        .0
    }
    fn vault_token_key(&self) -> Pubkey {
        zk_bridge::state::vault_token_pda(
            &bridge_program_id(),
            &settlement_program_id(),
            CHAIN_ID,
            &mint(),
        )
        .0
    }

    async fn start(&self) -> ProgramTestContext {
        let mut pt = rome_zk_testkit::program_test(
            &[
                rome_zk_testkit::ProgramSpec::new("zk_bridge", bridge_program_id()),
                rome_zk_testkit::ProgramSpec::new("spl_token", common::token_program_id()),
                rome_zk_testkit::ProgramSpec::new(
                    "spl_associated_token_account",
                    common::associated_token_program_id(),
                ),
            ],
            true,
        );
        pt.add_account(self.payer.pubkey(), funded_account(50_000_000_000));
        for (k, a) in &self.accounts {
            pt.add_account(*k, a.clone());
        }
        pt.start_with_context().await
    }

    /// `Deposit` by synthetic depositor `i` of the record's amount to the record's recipient, as the client
    /// builds it, for record number `index`.
    fn deposit_ix(&self, i: u64, index: u64, amount: u64, recipient: [u8; 20]) -> Instruction {
        zk_bridge_client::deposit_ix(
            &bridge_program_id(),
            &self.depositors[i as usize].pubkey(),
            &token_key(i),
            &settlement_program_id(),
            CHAIN_ID,
            &mint(),
            index,
            &fee_recipient_key(),
            amount,
            recipient,
        )
    }

    async fn send(
        &self,
        ctx: &mut ProgramTestContext,
        ix: Instruction,
        signer: Option<u64>,
        name: &str,
    ) -> Result<(), TransactionError> {
        let extra: Vec<&Keypair> = signer
            .map(|i| vec![&self.depositors[i as usize]])
            .unwrap_or_default();
        let (r, cu, _) = rome_zk_testkit::send_measuring_cu(ctx, &[ix], &self.payer, &extra).await;
        eprintln!("{name} consumed {cu} CU");
        r
    }

    async fn deposit_refused(&self, i: u64, ix: Instruction, want: BridgeError) {
        let mut ctx = self.start().await;
        let r = self.send(&mut ctx, ix, Some(i), "Deposit (refused)").await;
        assert_custom(r, want);
        // A refused deposit moves nothing.
        let t = get_account(&mut ctx, token_key(i)).await.unwrap();
        assert_eq!(decode_token_amount(&t.data), DEPOSITOR_TOKENS);
        let vault = get_account(&mut ctx, self.vault_token_key()).await.unwrap();
        assert_eq!(decode_token_amount(&vault.data), 0);
    }
}

async fn read_queue(ctx: &mut ProgramTestContext, key: Pubkey) -> queue_layout::DepositQueueFields {
    let a = get_account(ctx, key).await.expect("queue must exist");
    queue_layout::read(&a.data).unwrap()
}

async fn read_record(
    ctx: &mut ProgramTestContext,
    index: u64,
) -> deposit_record::DepositRecordFields {
    let key = deposit_record::pda(
        &bridge_program_id(),
        &settlement_program_id().to_bytes(),
        CHAIN_ID,
        index,
    )
    .0;
    let a = get_account(ctx, key).await.expect("record must exist");
    assert_eq!(a.owner, bridge_program_id());
    assert_eq!(a.data.len(), deposit_record::LEN);
    deposit_record::read(&a.data).unwrap()
}

async fn lamports(ctx: &mut ProgramTestContext, key: Pubkey) -> u64 {
    get_account(ctx, key).await.map(|a| a.lamports).unwrap_or(0)
}

// ------------------------------------------------------------------------------------------------
// Deposit: the golden values
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn three_deposits_reproduce_the_synthetic_batchs_records_and_hash_chain() {
    let mut rig = Rig::new();
    let h0 = seed_hash(&settlement_program_id(), CHAIN_ID);
    assert_eq!(
        h0,
        sidecar_hash("h_0"),
        "the queue's seed hash is the synthetic batch's h_0"
    );
    rig.set_queue_state(0, h0);
    let mut ctx = rig.start().await;

    let mut head = h0;
    let mut fee_total = 0;
    for i in 0..3u64 {
        let want = synthetic_record(i);
        let ix = rig.deposit_ix(i, i, want.amount_gwei, want.recipient);
        let name = format!("Deposit {i}");
        rig.send(&mut ctx, ix, Some(i), &name)
            .await
            .unwrap_or_else(|e| panic!("deposit {i} must succeed: {e:?}"));
        fee_total += FEE_LAMPORTS;

        let leaf = deposit::leaf(
            &soft_keccak,
            &settlement_program_id().to_bytes(),
            CHAIN_ID,
            i,
            &want.sender,
            &want.recipient,
            want.amount_gwei,
        );
        head = deposit::chain_next(&soft_keccak, &head, &leaf);

        let rec = read_record(&mut ctx, i).await;
        assert_eq!(rec.index, i);
        assert_eq!(rec.sender, want.sender, "the sender is the signer's wallet");
        assert_eq!(rec.recipient, want.recipient);
        assert_eq!(rec.amount_gwei, want.amount_gwei);
        assert_eq!(rec.hash_after, head, "record {i}'s hash_after");
        assert!(rec.enqueue_unix_ts > 0, "the record carries the clock");

        let q = read_queue(&mut ctx, rig.queue_key()).await;
        assert_eq!(q.count, i + 1);
        assert_eq!(q.head_hash, head);
    }
    assert_eq!(
        head,
        sidecar_hash("h_to"),
        "h_3 must equal the synthetic batch's chain end"
    );

    // The tokens are in the vault, the depositors paid, the fee recipient was paid.
    let total: u64 = (0..3).map(|i| synthetic_record(i).amount_gwei).sum();
    let vault = get_account(&mut ctx, rig.vault_token_key()).await.unwrap();
    assert_eq!(decode_token_amount(&vault.data), total);
    for i in 0..3u64 {
        let t = get_account(&mut ctx, token_key(i)).await.unwrap();
        assert_eq!(
            decode_token_amount(&t.data),
            DEPOSITOR_TOKENS - synthetic_record(i).amount_gwei
        );
    }
    assert_eq!(
        lamports(&mut ctx, fee_recipient_key()).await,
        rome_zk_testkit::rent_exempt(0) + fee_total
    );
}

#[tokio::test]
async fn deposit_adopts_a_prefunded_record_address() {
    let mut rig = Rig::new();
    let record_key = deposit_record::pda(
        &bridge_program_id(),
        &settlement_program_id().to_bytes(),
        CHAIN_ID,
        0,
    )
    .0;
    // Someone sent lamports to the address first.
    rig.put(record_key, funded_account(1_234_567));
    let mut ctx = rig.start().await;
    let want = synthetic_record(0);
    let ix = rig.deposit_ix(0, 0, want.amount_gwei, want.recipient);
    rig.send(
        &mut ctx,
        ix,
        Some(0),
        "Deposit (adopting a pre-funded record)",
    )
    .await
    .expect("a pre-funded record address must be adopted");
    let rec = read_record(&mut ctx, 0).await;
    assert_eq!(rec.sender, want.sender);
    assert_eq!(rec.amount_gwei, want.amount_gwei);
    let a = get_account(&mut ctx, record_key).await.unwrap();
    assert!(a.lamports >= rome_zk_testkit::rent_exempt(deposit_record::LEN));
    assert_eq!(read_queue(&mut ctx, rig.queue_key()).await.count, 1);
}

#[tokio::test]
async fn deposit_converts_a_six_decimal_amount_to_gwei() {
    let mut rig = Rig::new();
    let (vault_key, mut vault) = {
        let k = zk_bridge::state::vault_config_pda(
            &bridge_program_id(),
            &settlement_program_id(),
            CHAIN_ID,
        )
        .0;
        (k, rig.accounts[&k].clone())
    };
    vault.data[zk_bridge::state::vault_config::OFF_MINT_DECIMALS] = 6;
    rig.put(vault_key, vault);
    let mut ctx = rig.start().await;
    let ix = rig.deposit_ix(0, 0, 2_500_000, [0x42; 20]);
    rig.send(&mut ctx, ix, Some(0), "Deposit (6 decimals)")
        .await
        .unwrap();
    assert_eq!(
        read_record(&mut ctx, 0).await.amount_gwei,
        2_500_000_000,
        "2.5 tokens of a 6-decimal mint is 2.5e9 gwei"
    );
}

// ------------------------------------------------------------------------------------------------
// Deposit: refusals, by name
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn deposit_refuses_a_vault_config_of_another_settlement_program() {
    // A vault initialised under another settlement program sits at its own address, and no queue is there.
    let mut rig = Rig::new();
    let other = other_settlement();
    let bridge = bridge_program_id();
    let (cfg_key, cfg) = {
        let k = zk_bridge::state::vault_config_pda(&bridge, &other, CHAIN_ID).0;
        (
            k,
            vault_config_account(other, mint(), MINT_DECIMALS, Pubkey::new_unique()),
        )
    };
    rig.put(cfg_key, cfg);
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_VAULT_CONFIG] = AccountMeta::new_readonly(cfg_key, false);
    // The caller points the queue at the other settlement program's address too: nothing lives there.
    ix.accounts[D_QUEUE] = AccountMeta::new(
        queue_layout::pda(&bridge, &other.to_bytes(), CHAIN_ID).0,
        false,
    );
    rig.deposit_refused(0, ix, BridgeError::WrongDepositQueue)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_vault_config_of_another_settlement_program_with_the_canonical_queue() {
    let mut rig = Rig::new();
    let other = other_settlement();
    let cfg_key = zk_bridge::state::vault_config_pda(&bridge_program_id(), &other, CHAIN_ID).0;
    rig.put(
        cfg_key,
        vault_config_account(other, mint(), MINT_DECIMALS, Pubkey::new_unique()),
    );
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_VAULT_CONFIG] = AccountMeta::new_readonly(cfg_key, false);
    rig.deposit_refused(0, ix, BridgeError::WrongDepositQueue)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_vault_config_that_is_not_at_its_own_address() {
    // The canonical address, but the config inside names another settlement program.
    let mut rig = Rig::new();
    let cfg_key = zk_bridge::state::vault_config_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
    )
    .0;
    rig.put(
        cfg_key,
        vault_config_account(
            other_settlement(),
            mint(),
            MINT_DECIMALS,
            Pubkey::new_unique(),
        ),
    );
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::WrongVaultConfig)
        .await;
}

#[tokio::test]
async fn deposit_refuses_another_chains_queue() {
    let mut rig = Rig::new();
    let other_chain = CHAIN_ID + 1;
    rig.put_chain(&settlement_program_id(), other_chain);
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_QUEUE] = AccountMeta::new(
        queue_layout::pda(
            &bridge_program_id(),
            &settlement_program_id().to_bytes(),
            other_chain,
        )
        .0,
        false,
    );
    rig.deposit_refused(0, ix, BridgeError::WrongDepositQueue)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_reclaimed_chain_whose_root_is_gone() {
    let mut rig = Rig::new();
    // The chain was reclaimed: its root is no longer there (a closed account is owned by the system program).
    let root_key = rome_zk_layouts::root::pda(&settlement_program_id(), CHAIN_ID).0;
    rig.put(root_key, funded_account(0));
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::RootNotCanonical)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_reclaimed_chain_whose_registry_is_gone() {
    let mut rig = Rig::new();
    let registry_key = rome_zk_layouts::registry::pda(&settlement_program_id(), CHAIN_ID).0;
    rig.put(registry_key, funded_account(0));
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::RegistryNotCanonical)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_root_owned_by_another_program() {
    let mut rig = Rig::new();
    let root_key = rome_zk_layouts::root::pda(&settlement_program_id(), CHAIN_ID).0;
    let mut a = rome_zk_testkit::root_account_with_authority(
        CHAIN_ID,
        &Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    a.owner = Pubkey::new_unique();
    rig.put(root_key, a);
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::RootNotCanonical)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_root_at_another_address() {
    let mut rig = Rig::new();
    let other_root = Pubkey::new_unique();
    rig.put(
        other_root,
        rome_zk_testkit::root_account_with_authority(
            CHAIN_ID,
            &Pubkey::new_unique(),
            settlement_program_id(),
        ),
    );
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_ROOT] = AccountMeta::new_readonly(other_root, false);
    rig.deposit_refused(0, ix, BridgeError::RootNotCanonical)
        .await;
}

#[tokio::test]
async fn deposit_refuses_an_amount_below_the_minimum() {
    let rig = Rig::new();
    let ix = rig.deposit_ix(0, 0, MIN_AMOUNT - 1, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::DepositBelowMinimum)
        .await;
}

#[tokio::test]
async fn deposit_accepts_exactly_the_minimum() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    let ix = rig.deposit_ix(0, 0, MIN_AMOUNT, [0x42; 20]);
    rig.send(&mut ctx, ix, Some(0), "Deposit (the minimum)")
        .await
        .unwrap();
}

#[tokio::test]
async fn deposit_refuses_a_zero_recipient() {
    let rig = Rig::new();
    let ix = rig.deposit_ix(0, 0, 5_000, [0u8; 20]);
    rig.deposit_refused(0, ix, BridgeError::DepositRecipientInvalid)
        .await;
}

#[tokio::test]
async fn deposit_refuses_the_exit_portal_as_recipient() {
    let rig = Rig::new();
    // `exit_config_account` puts the portal at 0x11 repeated.
    let ix = rig.deposit_ix(0, 0, 5_000, [0x11u8; 20]);
    rig.deposit_refused(0, ix, BridgeError::DepositRecipientInvalid)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_fee_recipient_that_is_not_the_parameter() {
    let mut rig = Rig::new();
    let other = Pubkey::new_unique();
    rig.put(other, funded_account(rome_zk_testkit::rent_exempt(0)));
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_FEE_RECIPIENT] = AccountMeta::new(other, false);
    rig.deposit_refused(0, ix, BridgeError::WrongFeeRecipient)
        .await;
}

#[tokio::test]
async fn deposit_refuses_an_exit_config_that_names_another_bridge() {
    let mut rig = Rig::new();
    let key = rome_zk_layouts::exit::exit_config::pda(&settlement_program_id(), CHAIN_ID).0;
    let mut a = exit_config_account(settlement_program_id(), Pubkey::new_unique());
    let o = rome_zk_layouts::exit::exit_config::OFF_CHAIN_ID;
    a.data[o..o + 8].copy_from_slice(&CHAIN_ID.to_le_bytes());
    rig.put(key, a);
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::NotChainsBridge)
        .await;
}

#[tokio::test]
async fn deposit_refuses_an_exit_config_at_another_address() {
    let mut rig = Rig::new();
    let other = Pubkey::new_unique();
    rig.put(
        other,
        exit_config_account(settlement_program_id(), bridge_program_id()),
    );
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_EXIT_CONFIG] = AccountMeta::new_readonly(other, false);
    rig.deposit_refused(0, ix, BridgeError::ExitConfigNotCanonical)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_record_at_another_index() {
    let rig = Rig::new();
    // The queue's count is 0, the record account is the one for index 1.
    let ix = rig.deposit_ix(0, 1, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::WrongDepositRecord)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_record_address_that_already_holds_a_record() {
    let mut rig = Rig::new();
    let key = deposit_record::pda(
        &bridge_program_id(),
        &settlement_program_id().to_bytes(),
        CHAIN_ID,
        0,
    )
    .0;
    rig.put(key, record_account(0, &synthetic_record(0), [0x01; 32]));
    let ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    rig.deposit_refused(0, ix, BridgeError::DepositRecordInUse)
        .await;
}

#[tokio::test]
async fn deposit_refuses_a_missing_signature() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    // Same accounts, but the depositor does not sign.
    ix.accounts[0].is_signer = false;
    let (r, _, _) = rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &rig.payer, &[]).await;
    match r {
        Err(TransactionError::InstructionError(
            _,
            solana_sdk::instruction::InstructionError::MissingRequiredSignature,
        )) => {}
        other => panic!("expected MissingRequiredSignature, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------------------------
// Deposit with wrapped SOL
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_sol_deposit_wraps_and_deposits_in_one_transaction() {
    let native = zk_bridge_client::NATIVE_MINT;
    let bridge = bridge_program_id();
    let sp = settlement_program_id();
    let mut rig = Rig::new();
    // A chain whose vault holds wrapped SOL: the vault's token account is a native account.
    let vault_key = zk_bridge::state::vault_config_pda(&bridge, &sp, CHAIN_ID).0;
    let mut vault = rig.accounts[&vault_key].clone();
    vault.data
        [zk_bridge::state::vault_config::OFF_MINT..zk_bridge::state::vault_config::OFF_MINT + 32]
        .copy_from_slice(native.as_ref());
    rig.put(vault_key, vault);
    let (vault_authority, _) = zk_bridge::state::vault_authority_pda(&bridge, &sp, CHAIN_ID);
    let vault_token_key = zk_bridge::state::vault_token_pda(&bridge, &sp, CHAIN_ID, &native).0;
    let reserve = rome_zk_testkit::rent_exempt(zk_bridge::token::TOKEN_ACCOUNT_LEN);
    let mut vt = token_account(&native, &vault_authority, 0);
    vt.data[109..113].copy_from_slice(&1u32.to_le_bytes()); // is_native: Some(reserve)
    vt.data[113..121].copy_from_slice(&reserve.to_le_bytes());
    vt.lamports = reserve;
    rig.put(vault_token_key, vt);
    rig.put(native, mint_account(9));

    let mut ctx = rig.start().await;
    let amount = 3_000_000u64;
    let (wsol, mut ixs) = zk_bridge_client::wrap_sol_ixs(&rig.depositors[0].pubkey(), amount);
    ixs.push(zk_bridge_client::deposit_ix(
        &bridge,
        &rig.depositors[0].pubkey(),
        &wsol,
        &sp,
        CHAIN_ID,
        &native,
        0,
        &fee_recipient_key(),
        amount,
        [0x42; 20],
    ));
    let before = lamports(&mut ctx, rig.depositors[0].pubkey()).await;
    let (r, cu, _) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &ixs, &rig.payer, &[&rig.depositors[0]]).await;
    r.expect("wrap and deposit must succeed");
    eprintln!("wrap SOL + Deposit (one transaction) consumed {cu} CU");

    let rec = read_record(&mut ctx, 0).await;
    assert_eq!(rec.amount_gwei, amount);
    assert_eq!(rec.sender, rig.depositors[0].pubkey().to_bytes());
    let vt = get_account(&mut ctx, vault_token_key).await.unwrap();
    assert_eq!(decode_token_amount(&vt.data), amount);
    assert_eq!(vt.lamports, reserve + amount, "the vault holds the SOL");
    let after = lamports(&mut ctx, rig.depositors[0].pubkey()).await;
    assert!(before - after >= amount + FEE_LAMPORTS);
}

// ------------------------------------------------------------------------------------------------
// CloseDeposit
// ------------------------------------------------------------------------------------------------

fn cursor_key() -> Pubkey {
    rome_zk_layouts::cursor::pda(&inbox_program_id(), &settlement_program_id(), CHAIN_ID).0
}

fn record_key(chain_id: u64, index: u64) -> Pubkey {
    deposit_record::pda(
        &bridge_program_id(),
        &settlement_program_id().to_bytes(),
        chain_id,
        index,
    )
    .0
}

/// A rig with two records of the chain in place (indices 0 and 1, senders the first two synthetic
/// depositors) and a cursor that has credited `next` of them, `final_` of them final.
fn close_rig(next: u64, final_: u64) -> Rig {
    let mut rig = Rig::new();
    for i in 0..2u64 {
        rig.put(
            record_key(CHAIN_ID, i),
            record_account(i, &synthetic_record(i), [0x10 + i as u8; 32]),
        );
    }
    rig.put(
        cursor_key(),
        cursor_v2(inbox_program_id(), CHAIN_ID, next, final_),
    );
    rig
}

fn close_ix(index: u64) -> Instruction {
    zk_bridge_client::close_deposit_ix(
        &bridge_program_id(),
        &inbox_program_id(),
        &settlement_program_id(),
        CHAIN_ID,
        index,
        &Pubkey::new_from_array(synthetic_record(index).sender),
    )
}

async fn close_refused(rig: &Rig, ix: Instruction, want: BridgeError) {
    let mut ctx = rig.start().await;
    let r = rig.send(&mut ctx, ix, None, "CloseDeposit (refused)").await;
    assert_custom(r, want);
}

#[tokio::test]
async fn close_deposit_refunds_the_rent_to_the_sender_once_the_batch_is_final() {
    let rig = close_rig(2, 2);
    let sender = Pubkey::new_from_array(synthetic_record(0).sender);
    let mut ctx = rig.start().await;
    let rent = lamports(&mut ctx, record_key(CHAIN_ID, 0)).await;
    let before = lamports(&mut ctx, sender).await;
    rig.send(&mut ctx, close_ix(0), None, "CloseDeposit")
        .await
        .expect("a final, credited deposit must close");
    assert_eq!(lamports(&mut ctx, sender).await, before + rent);
    let gone = get_account(&mut ctx, record_key(CHAIN_ID, 0)).await;
    assert!(
        gone.map(|a| a.lamports == 0 || a.data.is_empty())
            .unwrap_or(true),
        "the record is closed"
    );
    // The other record is untouched.
    assert_eq!(read_record(&mut ctx, 1).await.index, 1);
}

#[tokio::test]
async fn close_deposit_is_refused_until_the_crediting_batch_is_final_then_allowed() {
    let rig = Rig::new();
    let mut ctx = rig.start().await;
    // A real deposit, then a cursor moved along step by step.
    let want = synthetic_record(0);
    rig.send(
        &mut ctx,
        rig.deposit_ix(0, 0, want.amount_gwei, want.recipient),
        Some(0),
        "Deposit",
    )
    .await
    .unwrap();
    let sender = rig.depositors[0].pubkey();
    let vault_before = decode_token_amount(
        &get_account(&mut ctx, rig.vault_token_key())
            .await
            .unwrap()
            .data,
    );
    let fee_before = lamports(&mut ctx, fee_recipient_key()).await;
    assert_eq!(vault_before, want.amount_gwei);

    // No batch has taken the deposit in yet.
    ctx.set_account(
        &cursor_key(),
        &cursor_v2(inbox_program_id(), CHAIN_ID, 0, 0).into(),
    );
    let r = rig
        .send(&mut ctx, close_ix(0), None, "CloseDeposit (not credited)")
        .await;
    assert_custom(r, BridgeError::DepositNotCredited);

    // A batch has credited it, and it is not final.
    ctx.set_account(
        &cursor_key(),
        &cursor_v2(inbox_program_id(), CHAIN_ID, 1, 0).into(),
    );
    let r = rig
        .send(&mut ctx, close_ix(0), None, "CloseDeposit (not final)")
        .await;
    assert_custom(r, BridgeError::DepositNotFinal);
    assert!(get_account(&mut ctx, record_key(CHAIN_ID, 0))
        .await
        .is_some());

    // The crediting batch is final.
    ctx.set_account(
        &cursor_key(),
        &cursor_v2(inbox_program_id(), CHAIN_ID, 1, 1).into(),
    );
    let record_rent = lamports(&mut ctx, record_key(CHAIN_ID, 0)).await;
    let sender_before = lamports(&mut ctx, sender).await;
    rig.send(&mut ctx, close_ix(0), None, "CloseDeposit")
        .await
        .expect("allowed once the crediting batch is final");
    assert_eq!(
        lamports(&mut ctx, sender).await,
        sender_before + record_rent
    );

    // Closing the record moves no token and pays no fee: the vault and the fee recipient are as they were.
    let vault_after = decode_token_amount(
        &get_account(&mut ctx, rig.vault_token_key())
            .await
            .unwrap()
            .data,
    );
    assert_eq!(vault_after, vault_before);
    assert_eq!(lamports(&mut ctx, fee_recipient_key()).await, fee_before);
    assert_eq!(
        fee_before,
        rome_zk_testkit::rent_exempt(0) + FEE_LAMPORTS,
        "the fee was paid by the deposit"
    );
}

#[tokio::test]
async fn close_deposit_refuses_an_index_the_final_cursor_has_not_reached() {
    // Two credited, one final: index 1 is credited but not final, index 0 closes.
    let rig = close_rig(2, 1);
    close_refused(&rig, close_ix(1), BridgeError::DepositNotFinal).await;
    let mut ctx = rig.start().await;
    rig.send(&mut ctx, close_ix(0), None, "CloseDeposit")
        .await
        .unwrap();
}

#[tokio::test]
async fn close_deposit_refuses_a_cursor_at_another_address() {
    let mut rig = close_rig(2, 2);
    let other = Pubkey::new_unique();
    rig.put(other, cursor_v2(inbox_program_id(), CHAIN_ID, 2, 2));
    let mut ix = close_ix(0);
    ix.accounts[C_CURSOR] = AccountMeta::new_readonly(other, false);
    close_refused(&rig, ix, BridgeError::WrongCursor).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_cursor_owned_by_another_program() {
    let mut rig = close_rig(2, 2);
    rig.put(
        cursor_key(),
        cursor_v2(Pubkey::new_unique(), CHAIN_ID, 2, 2),
    );
    close_refused(&rig, close_ix(0), BridgeError::WrongCursor).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_version_one_cursor() {
    let mut rig = close_rig(2, 2);
    rig.put(
        cursor_key(),
        rome_zk_testkit::cursor_account_for(1, inbox_program_id(), CHAIN_ID, 1),
    );
    close_refused(&rig, close_ix(0), BridgeError::WrongCursor).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_cursor_of_another_chain() {
    let mut rig = close_rig(2, 2);
    rig.put(
        cursor_key(),
        cursor_v2(inbox_program_id(), CHAIN_ID + 1, 2, 2),
    );
    close_refused(&rig, close_ix(0), BridgeError::WrongCursor).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_record_of_another_chain() {
    let mut rig = close_rig(2, 2);
    // A record of the same index under another chain, with the same sender and a closeable cursor.
    let other_chain = CHAIN_ID + 1;
    let other_key = record_key(other_chain, 0);
    rig.put(
        other_key,
        record_account(0, &synthetic_record(0), [0x10; 32]),
    );
    let mut ix = close_ix(0);
    ix.accounts[C_RECORD] = AccountMeta::new(other_key, false);
    close_refused(&rig, ix, BridgeError::WrongDepositRecord).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_record_at_another_index() {
    let rig = close_rig(2, 2);
    // Asked to close index 0 with index 1's record.
    let mut ix = close_ix(0);
    ix.accounts[C_RECORD] = AccountMeta::new(record_key(CHAIN_ID, 1), false);
    close_refused(&rig, ix, BridgeError::WrongDepositRecord).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_record_owned_by_another_program() {
    let mut rig = close_rig(2, 2);
    let mut a = record_account(0, &synthetic_record(0), [0x10; 32]);
    a.owner = Pubkey::new_unique();
    rig.put(record_key(CHAIN_ID, 0), a);
    close_refused(&rig, close_ix(0), BridgeError::WrongDepositRecord).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_rent_recipient_that_is_not_the_sender() {
    let mut rig = close_rig(2, 2);
    let other = Pubkey::new_unique();
    rig.put(other, funded_account(1));
    let mut ix = close_ix(0);
    ix.accounts[C_RENT_RECIPIENT] = AccountMeta::new(other, false);
    close_refused(&rig, ix, BridgeError::WrongRentRecipient).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_registry_naming_another_inbox() {
    let mut rig = close_rig(2, 2);
    let key = rome_zk_layouts::registry::pda(&settlement_program_id(), CHAIN_ID).0;
    rig.put(
        key,
        registry_account(CHAIN_ID, &Pubkey::new_unique(), settlement_program_id()),
    );
    close_refused(&rig, close_ix(0), BridgeError::WrongInboxProgram).await;
}

#[tokio::test]
async fn close_deposit_refuses_a_registry_at_another_address() {
    let mut rig = close_rig(2, 2);
    let other = Pubkey::new_unique();
    rig.put(
        other,
        registry_account(CHAIN_ID, &inbox_program_id(), settlement_program_id()),
    );
    let mut ix = close_ix(0);
    ix.accounts[C_REGISTRY] = AccountMeta::new_readonly(other, false);
    close_refused(&rig, ix, BridgeError::RegistryNotCanonical).await;
}

// The vault token check is what keeps a credit backed: naming any other token account of the vault's mint, the
// depositor's own or another chain's vault, would queue a record whose tokens never reached this chain's vault.
#[tokio::test]
async fn deposit_refuses_a_vault_token_at_another_address() {
    let mut rig = Rig::new();
    let decoy = Pubkey::new_unique();
    rig.put(
        decoy,
        token_account(&mint(), &rig.depositors[0].pubkey(), 0),
    );
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_VAULT_TOKEN] = AccountMeta::new(decoy, false);
    rig.deposit_refused(0, ix, BridgeError::WrongVaultToken)
        .await;
}

#[tokio::test]
async fn deposit_refuses_another_chains_vault_token() {
    let mut rig = Rig::new();
    rig.put_chain(&settlement_program_id(), CHAIN_ID + 1);
    let other = zk_bridge::state::vault_token_pda(
        &bridge_program_id(),
        &settlement_program_id(),
        CHAIN_ID + 1,
        &mint(),
    )
    .0;
    let mut ix = rig.deposit_ix(0, 0, 5_000, [0x42; 20]);
    ix.accounts[D_VAULT_TOKEN] = AccountMeta::new(other, false);
    rig.deposit_refused(0, ix, BridgeError::WrongVaultToken)
        .await;
}
