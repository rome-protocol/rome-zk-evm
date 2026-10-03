//! A restart in the middle of a batch keeps settlement able to post that batch.
//!
//! A crash between `OpenBatch` and `FinalizeBatch`, then a restart, must NOT cost settlement a batch id.
//! Settlement posts exactly `head_pending_batch + 1` (`PostRoot` refuses anything else with
//! `BadBatchSequence`) and that id's inbox account must be owned by the inbox program and finalized
//! (`WrongInboxAccount` otherwise). An id the inbox cursor has passed can never be opened again, so a
//! startup that abandons the half-written batch leaves settlement waiting for an id that can never exist:
//! the chain halts for good.
//!
//! Both REAL programs are loaded (`zk_settlement` upgradeable, `zk_inbox` plain), the chain is registered
//! with the real `InitChain`, and the "restarted batcher" is `pipeline::startup_recover` — the one library
//! function the binary calls at startup — so this test's text is the same before and after the fix; the
//! fix only replaces that function's body.
//!
//! The test asserts the SAFE behaviour: after the restart, settlement's `PostRoot` for
//! `head_pending_batch + 1` succeeds, the inbox batch for that id is finalized, and no `AbandonBatch` was
//! ever sent.

use alloy::primitives::B256;
use alloy::signers::local::PrivateKeySigner;
use rome_zk_batcher::anchor::{self, Anchor};
use rome_zk_batcher::channel::{self, Block};
use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{self, BatchTarget, StartupRecover, WindowConfig};
use rome_zk_batcher::resolve::{self, AccountOps, ResolveError, ResolveOutcome};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_batcher::sink::ChannelPostRootSink;
use rome_zk_batcher::source::BlockSource;
use rome_zk_layouts::registry as reg_layout;
use rome_zk_sequencer::header::SubBlockHeader;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::sealer::SUB_BLOCKS_PER_BLOCK;
use rome_zk_sequencer::signing::sign_header;
use rome_zk_sequencer::testutil::signed_raw_tx;
use rome_zk_testkit::{cursor_account, funded_keypair, rent_exempt};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_program::{clock::Clock, instruction::Instruction, keccak, pubkey::Pubkey};
use solana_program_test::{BanksClient, ProgramTestContext};
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::tempdir;
use zk_settlement_client as sclient;

/// A reserved chain id (`< 2^32`): the unproved `PostRoot` this test drives is only allowed there.
const CHAIN_ID: u64 = 1;
const BLOCK_GAS_LIMIT: u64 = 100_000_000;
/// The block timestamps the log below produces sit within a few seconds of this; the bank clock is pinned
/// right after it so a block's drift against `open_unix_ts` is deterministic (the resume search checks it).
const CLOCK_UNIX_TS: i64 = 1_757_000_010;
/// Small enough that three blocks of signed transactions cut into at least three frames.
const MAX_FRAME_BODY_LEN: usize = 2_000;
const GENESIS_BLOCK_HASH: [u8; 32] = [0x11u8; 32];

#[derive(Clone)]
struct BanksAccountOps {
    banks_client: BanksClient,
}

impl AccountOps for BanksAccountOps {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        let bc = self.banks_client.clone();
        let account = bc
            .get_account(*pubkey)
            .await
            .expect("BanksClient::get_account");
        Ok(account.map(|a| a.data))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        let mut out = Vec::with_capacity(pubkeys.len());
        for pubkey in pubkeys {
            out.push(self.get_account(pubkey).await?.is_some());
        }
        Ok(out)
    }
}

/// [`Sender`] bridged to a real `BanksClient`. It records every instruction it sends (so the test can
/// prove no `AbandonBatch` ever went out) and makes each transaction unique with a rising priority fee
/// (the bank's blockhash does not advance on its own, so two byte-identical sends would otherwise be
/// refused as already processed).
struct BanksSender {
    banks_client: BanksClient,
    payer: Keypair,
    nonce: AtomicU64,
    sent: Mutex<Vec<Instruction>>,
}

impl BanksSender {
    fn new(banks_client: BanksClient, payer: &Keypair) -> Self {
        Self {
            banks_client,
            payer: payer.insecure_clone(),
            nonce: AtomicU64::new(0),
            sent: Mutex::new(Vec::new()),
        }
    }

    fn sent(&self) -> Vec<Instruction> {
        self.sent.lock().unwrap().clone()
    }
}

impl Sender for BanksSender {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        self.sent.lock().unwrap().extend_from_slice(instructions);
        let n = self.nonce.fetch_add(1, Ordering::Relaxed);
        let mut ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(tuning.compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(
                tuning.priority_fee_micro_lamports + n,
            ),
        ];
        ixs.extend_from_slice(instructions);
        let mut banks_client = self.banks_client.clone();
        let recent = banks_client
            .get_latest_blockhash()
            .await
            .expect("get_latest_blockhash");
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&self.payer.pubkey()),
            &[&self.payer],
            recent,
        );
        let sig = tx.signatures[0];
        rome_zk_testkit::send_checked(&mut banks_client, tx)
            .await
            .unwrap_or_else(|e| panic!("transaction failed: {e}"));
        let bytes: [u8; 64] = sig.into();
        Ok(solana_signature::Signature::from(bytes))
    }
}

fn tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 1_400_000,
        loaded_accounts_data_size_limit: 131_072,
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

fn funded_account() -> Account {
    Account {
        lamports: 50_000_000_000,
        data: vec![],
        owner: system_program::id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// The real chain registration, copied from `programs/zk-settlement/tests/settlement.rs`
/// (`default_chain`, `ensure_global_config`, `init_chain`) in the same shape: global config, the reserved
/// allowlist marker, then the reserved-path `InitChain`.
struct Chain {
    settlement_program: Pubkey,
    inbox_program: Pubkey,
    authority: Keypair,
    genesis_state_root: [u8; 32],
    registry_authority: Keypair,
    treasury: Pubkey,
    upgrade_authority: Keypair,
}

async fn send(
    ctx: &mut ProgramTestContext,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) {
    let (result, _cu, logs) =
        rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, extra_signers).await;
    result.unwrap_or_else(|e| panic!("setup transaction failed: {e:?}\n{logs:#?}"));
}

async fn register_chain(ctx: &mut ProgramTestContext, payer: &Keypair, c: &Chain) {
    // `InitGlobalConfig` checks the program's real upgrade authority: patch the `ProgramData` account
    // (seeded with `Pubkey::default()`) to this key — a data-only rewrite, same as settlement.rs.
    let program_data = sclient::program_data_pda(&c.settlement_program);
    let mut pd_account = ctx
        .banks_client
        .get_account(program_data)
        .await
        .unwrap()
        .expect("ProgramData account must exist");
    pd_account.data[13..45].copy_from_slice(c.upgrade_authority.pubkey().as_ref());
    ctx.set_account(&program_data, &pd_account.into());

    // The treasury must be rent-exempt before `InitGlobalConfig`, funded by a real transfer.
    let funder = ctx.payer.insecure_clone();
    let transfer = solana_system_interface::instruction::transfer(
        &funder.pubkey(),
        &c.treasury,
        rent_exempt(0),
    );
    send(ctx, &[transfer], &funder, &[]).await;

    let fields = sclient::GlobalConfigFields {
        registry_authority: c.registry_authority.pubkey(),
        treasury: c.treasury,
        permissionless_init_enabled: true,
        // The program floors this at `governance::MIN_RECLAIM_WINDOW_SLOTS` (216_000 slots, one day).
        reclaim_window_slots: 216_000,
        deposit_lamports: 2_000_000_000,
        default_fee_base_lamports: 1_000_000,
        default_fee_bps: 0,
    };
    let ix = sclient::init_global_config_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.upgrade_authority.pubkey(),
        fields.clone(),
    );
    send(ctx, &[ix], payer, &[&c.upgrade_authority]).await;
    let set_ix = sclient::set_global_config_ix(
        &c.settlement_program,
        &c.registry_authority.pubkey(),
        sclient::GlobalConfigUpdate {
            permissionless_init_enabled: fields.permissionless_init_enabled,
            reclaim_window_slots: fields.reclaim_window_slots,
            deposit_lamports: fields.deposit_lamports,
            default_fee_base_lamports: fields.default_fee_base_lamports,
            default_fee_bps: fields.default_fee_bps,
        },
    );
    send(ctx, &[set_ix], payer, &[&c.registry_authority]).await;

    let allow = sclient::allow_reserved_id_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.registry_authority.pubkey(),
        CHAIN_ID,
    );
    send(ctx, &[allow], payer, &[&c.registry_authority]).await;

    let init = sclient::init_chain_reserved_ix(
        &c.settlement_program,
        &payer.pubkey(),
        &c.authority.pubkey(),
        &c.registry_authority.pubkey(),
        CHAIN_ID,
        sclient::InitChainFields {
            number: 0,
            parent_hash: [0u8; 32],
            state_root: c.genesis_state_root,
            block_hash: GENESIS_BLOCK_HASH,
            profile: 0,
            challenge_window_slots: 5,
            prove_window_slots: 1000,
            proving_policy: 1,
            poster_bond: 0,
            exit_cap_per_window: 0,
            max_pending: 16,
            inbox_program: c.inbox_program,
            registry_entries: vec![
                sclient::RegistryEntry {
                    curve: reg_layout::CURVE_BN254,
                    scheme: reg_layout::SCHEME_PLONK,
                    vkey_hash: [0x11u8; 32],
                    layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
                },
                sclient::RegistryEntry {
                    curve: reg_layout::CURVE_BN254,
                    scheme: reg_layout::SCHEME_GROTH16,
                    vkey_hash: [0u8; 32],
                    layout_id: reg_layout::LAYOUT_HEADER_FALLBACK,
                },
            ],
            max_drift_secs: 60,
        },
    );
    send(ctx, &[init], payer, &[&c.authority, &c.registry_authority]).await;
}

/// Appends `count` blocks starting at `start_block` to the ordered log at `dir` (a real `LogWriter`,
/// `SUB_BLOCKS_PER_BLOCK` sub-blocks per block, one signed tx per sub-block).
fn write_blocks(dir: &Path, start_block: u64, count: u64) {
    let sender = PrivateKeySigner::random();
    let mut writer = LogWriter::open(dir, 10_000).unwrap();
    let mut prev_hash = B256::ZERO;
    for block in start_block..start_block + count {
        for index in 0..SUB_BLOCKS_PER_BLOCK {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block,
                index,
                timestamp_us: 1_757_000_000_000_000
                    + (block * SUB_BLOCKS_PER_BLOCK as u64 + index as u64) * 50_000,
                tx_root: B256::repeat_byte(index as u8),
                receipts_root: B256::repeat_byte(index as u8 + 1),
                gas_used: 21_000,
                prev_hash,
                deposits_end: None,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let tx = signed_raw_tx(&sender, CHAIN_ID, block * 1_000 + index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }
    }
}

fn read_blocks(log_dir: &Path, anchor: &Anchor) -> Vec<Block> {
    let mut source = BlockSource::open(
        log_dir,
        CHAIN_ID,
        BLOCK_GAS_LIMIT,
        SUB_BLOCKS_PER_BLOCK,
        anchor.from_block,
        anchor.prev_block_timestamp_secs,
    )
    .unwrap();
    let mut blocks = Vec::new();
    while let Some(sourced) = source.next_block().unwrap() {
        blocks.push(sourced.block);
    }
    blocks
}

async fn read_batch(
    banks_client: &BanksClient,
    inbox: &Pubkey,
    settlement: &Pubkey,
    batch: u64,
) -> Option<zk_inbox_client::BatchAccount> {
    let (pda, _) = zk_inbox_client::batch_pda(inbox, settlement, CHAIN_ID, batch);
    let acct = banks_client.clone().get_account(pda).await.unwrap()?;
    if acct.owner != *inbox || acct.data.is_empty() {
        return None;
    }
    Some(zk_inbox_client::decode_batch_account(&acct.data).unwrap())
}

/// Posts one group end to end under the next cursor id — what the batcher's run loop does for whatever
/// the startup recovery left behind.
async fn post_group(
    sender: &BanksSender,
    accounts: &BanksAccountOps,
    inbox: Pubkey,
    settlement: Pubkey,
    payer: Pubkey,
    group: &[Block],
) -> u64 {
    let compressed = channel::encode_stream(group);
    pipeline::re_derive_and_check(group, &compressed).unwrap();
    let expected_next = resolve::read_cursor_next_batch(accounts, &inbox, &settlement, CHAIN_ID)
        .await
        .unwrap();
    let batch = match resolve::resolve_batch_id(
        accounts,
        &inbox,
        &settlement,
        CHAIN_ID,
        &compressed,
        MAX_FRAME_BODY_LEN,
        expected_next,
    )
    .await
    .unwrap()
    {
        ResolveOutcome::PostUnder(b) => b,
        ResolveOutcome::AlreadyPosted(b) => return b,
    };
    let frames = channel::cut_frames(CHAIN_ID, batch, &compressed, MAX_FRAME_BODY_LEN);
    let open = zk_inbox_client::open_and_grow_batch_ixs(
        &inbox,
        &payer,
        CHAIN_ID,
        batch,
        frames.len() as u32,
        &settlement,
    );
    sender.send_and_confirm(&open, tuning()).await.unwrap();
    let target = BatchTarget {
        program_id: inbox,
        settlement_program: settlement,
        payer,
        chain_id: CHAIN_ID,
        batch,
    };
    for stages in &pipeline::build_frame_jobs(target, &frames) {
        for stage in stages {
            for ixs in stage {
                sender.send_and_confirm(ixs, tuning()).await.unwrap();
            }
        }
    }
    let finalize =
        zk_inbox_client::finalize_batch_ix(&inbox, &payer, &settlement, CHAIN_ID, batch, 0);
    sender
        .send_and_confirm(std::slice::from_ref(&finalize), tuning())
        .await
        .unwrap();
    batch
}

#[tokio::test]
async fn a_restart_mid_batch_keeps_settlement_able_to_post_that_batch() {
    let settlement_program = Pubkey::new_unique();
    let inbox_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox_program),
        ],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    let c = Chain {
        settlement_program,
        inbox_program,
        authority: funded_keypair(),
        genesis_state_root: keccak::hashv(&[b"genesis"]).to_bytes(),
        registry_authority: funded_keypair(),
        treasury: Pubkey::new_unique(),
        upgrade_authority: funded_keypair(),
    };
    pt.add_account(c.authority.pubkey(), funded_account());
    // Batch ids are 1-based: the cursor starts at 1, so the first batch this chain opens is id 1.
    pt.add_account(
        zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, CHAIN_ID).0,
        cursor_account(inbox_program, CHAIN_ID, 1),
    );
    let mut ctx = pt.start_with_context().await;
    let mut clock: Clock = ctx.banks_client.get_sysvar().await.unwrap();
    clock.unix_timestamp = CLOCK_UNIX_TS;
    ctx.set_sysvar(&clock);
    register_chain(&mut ctx, &payer, &c).await;

    let log_dir = tempdir().unwrap();
    write_blocks(log_dir.path(), 1, 3);
    let accounts = BanksAccountOps {
        banks_client: ctx.banks_client.clone(),
    };
    let window_cfg = WindowConfig {
        inbox_program_id: inbox_program,
        settlement_program_id: settlement_program,
        payer: c.authority.pubkey(),
        chain_id: CHAIN_ID,
        max_frame_body_len: MAX_FRAME_BODY_LEN,
        chunk_tuning: tuning(),
        open_tuning: tuning(),
        chunk_retry_compute_unit_limit: 1_400_000,
        finalize_tuning: tuning(),
        finalize_poll_interval: Duration::from_millis(1),
        finalize_max_polls: 200,
        in_flight_frames: 8,
        confirm_poll_interval: Duration::from_millis(1),
        signature_status_batch_size: 32,
        batches_in_flight: 2,
        cu_sample: None,
        cu_sample_every: 1,
    };
    let startup_cfg = StartupRecover {
        window: &window_cfg,
        log_dir: log_dir.path(),
        sub_blocks_per_block: SUB_BLOCKS_PER_BLOCK,
        block_gas_limit: BLOCK_GAS_LIMIT,
        blocks_per_batch: 10,
    };
    let metrics = Metrics::new();
    let (sink, _sink_rx) = ChannelPostRootSink::new();

    // ===== The first process: groups blocks 1..=3, opens batch 1, seals ONE frame, then crashes. =====
    let first_process = BanksSender::new(ctx.banks_client.clone(), &c.authority);
    let anchor0 = anchor::resolve_anchor(
        &accounts,
        &inbox_program,
        &settlement_program,
        CHAIN_ID,
        log_dir.path(),
        SUB_BLOCKS_PER_BLOCK,
        BLOCK_GAS_LIMIT,
    )
    .await
    .unwrap();
    assert_eq!(anchor0.from_block, 1);
    let blocks = read_blocks(log_dir.path(), &anchor0);
    assert_eq!(blocks.len(), 3, "the log holds exactly 3 complete blocks");
    let compressed = channel::encode_stream(&blocks);
    let frames = channel::cut_frames(CHAIN_ID, 1, &compressed, MAX_FRAME_BODY_LEN);
    assert!(
        frames.len() >= 3,
        "the fixture needs at least 3 frames to leave a batch half written, got {}",
        frames.len()
    );
    let open = zk_inbox_client::open_and_grow_batch_ixs(
        &inbox_program,
        &c.authority.pubkey(),
        CHAIN_ID,
        1,
        frames.len() as u32,
        &settlement_program,
    );
    first_process
        .send_and_confirm(&open, tuning())
        .await
        .unwrap();
    let chunk0 = pipeline::plan_chunk(
        &inbox_program,
        &c.authority.pubkey(),
        &settlement_program,
        CHAIN_ID,
        1,
        0,
        &frames[0].to_bytes(),
    );
    first_process
        .send_and_confirm(&chunk0, tuning())
        .await
        .unwrap();

    let crashed = read_batch(&ctx.banks_client, &inbox_program, &settlement_program, 1)
        .await
        .expect("batch 1 must be open");
    assert!(!crashed.finalized, "the crash leaves batch 1 unfinalized");
    assert_eq!(crashed.leaves_present, 1, "only frame 0 was sealed");
    assert_eq!(crashed.expected_count as usize, frames.len());
    drop(first_process);

    // ===== The restart: a new process, the same startup recovery the binary runs. =====
    let restarted = BanksSender::new(ctx.banks_client.clone(), &c.authority);
    let anchor = pipeline::startup_recover(&accounts, &restarted, &metrics, &sink, &startup_cfg)
        .await
        .unwrap_or_else(|e| panic!("startup recovery must not fail: {e}"));

    // Whatever blocks recovery left unposted, the run loop posts under the next cursor id.
    let leftover = read_blocks(log_dir.path(), &anchor);
    if !leftover.is_empty() {
        post_group(
            &restarted,
            &accounts,
            inbox_program,
            settlement_program,
            c.authority.pubkey(),
            &leftover,
        )
        .await;
    }

    // ===== Settlement: it must still be able to post batch 1, the id the chain is waiting for. =====
    let (root_pda, _) = sclient::root_pda(&settlement_program, CHAIN_ID);
    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        root.head_pending_batch, 0,
        "nothing posted to settlement yet"
    );
    let next_to_settle = root.head_pending_batch + 1;
    assert_eq!(next_to_settle, 1);

    let batch1 = read_batch(&ctx.banks_client, &inbox_program, &settlement_program, 1).await;
    let acc = batch1.as_ref().map(|b| b.acc).unwrap_or_default();
    let post_root = sclient::post_root_ix(
        &settlement_program,
        &c.authority.pubkey(),
        &inbox_program,
        &c.treasury,
        sclient::PostRootFields {
            chain_id: CHAIN_ID,
            batch: next_to_settle,
            prev_batch: 0,
            pre_state_root: c.genesis_state_root,
            first_block: 1,
            last_block: 3,
            state_root: keccak::hashv(&[b"state root 1"]).to_bytes(),
            block_roots_merkle: keccak::hashv(&[b"block roots 1"]).to_bytes(),
            inbox_commitment: acc,
            forced_outcome_commitment: rome_zk_layouts::forced_empty_root(
                &rome_zk_merkle::keccak256,
            ),
            parent_hash: keccak::hashv(&[b"parent hash 1"]).to_bytes(),
            last_block_hash: keccak::hashv(&[b"block hash 1"]).to_bytes(),
            gas_in_batch: 0,
        },
    )
    .expect("reserved chain");
    let (result, _cu, logs) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[post_root], &c.authority, &[]).await;
    result.unwrap_or_else(|e| {
        panic!(
            "settlement cannot post batch {next_to_settle} after the restart: {e:?}\n\
             (batch 1's inbox account is {}; a restart that abandons a half-written batch burns the \
             one id settlement is waiting for)\n{logs:#?}",
            if batch1.is_some() {
                "present"
            } else {
                "gone (abandoned, system-owned)"
            }
        )
    });

    let root = sclient::decode_root_account(
        &ctx.banks_client
            .get_account(root_pda)
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(root.head_pending_batch, 1);

    // The inbox batch for head_pending_batch + 1 ended up finalized, not abandoned ...
    let batch1 = batch1.expect("batch 1 must still exist");
    assert!(batch1.finalized, "batch 1 must be finalized");
    pipeline::verify_acc(&batch1, &frames).unwrap_or_else(|e| {
        panic!("batch 1's on-chain acc must match the frames the first process started: {e}")
    });

    // ... no AbandonBatch was ever sent by the restarted process ...
    let abandon_disc = zk_inbox_client::abandon_batch_ix(
        &inbox_program,
        &c.authority.pubkey(),
        &settlement_program,
        CHAIN_ID,
        1,
    )
    .data[0];
    let abandons: Vec<_> = restarted
        .sent()
        .into_iter()
        .filter(|ix| ix.program_id == inbox_program && ix.data.first() == Some(&abandon_disc))
        .collect();
    assert!(
        abandons.is_empty(),
        "the restarted batcher sent {} AbandonBatch instruction(s)",
        abandons.len()
    );

    // ... and the cursor never moved past the one id the first process opened.
    let cursor = zk_inbox_client::decode_batch_cursor(
        &ctx.banks_client
            .get_account(
                zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, CHAIN_ID).0,
            )
            .await
            .unwrap()
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        cursor.next_batch, 2,
        "the restart must finish batch 1, not open a replacement"
    );
}

// ===================== resume cases: shapes of a half-written batch =====================

use rome_zk_batcher::channel::Frame;
use rome_zk_batcher::pipeline::{PipelineError, StartupError};

/// Writes `blocks` (block number, txs in that block) to one log; tx `i` sits in sub-block `i`.
fn write_log(dir: &Path, blocks: &[(u64, u16)]) {
    let sender = PrivateKeySigner::random();
    let mut writer = LogWriter::open(dir, 10_000).unwrap();
    let mut prev_hash = B256::ZERO;
    for &(block, n_txs) in blocks {
        for index in 0..SUB_BLOCKS_PER_BLOCK {
            let header = SubBlockHeader {
                chain_id: CHAIN_ID,
                block,
                index,
                timestamp_us: 1_757_000_000_000_000
                    + (block * SUB_BLOCKS_PER_BLOCK as u64 + index as u64) * 50_000,
                tx_root: B256::repeat_byte(index as u8),
                receipts_root: B256::repeat_byte(index as u8 + 1),
                gas_used: 21_000,
                prev_hash,
                deposits_end: None,
            };
            let signature = sign_header(&PrivateKeySigner::random(), &header);
            let txs: Vec<_> = if index < n_txs {
                vec![signed_raw_tx(
                    &sender,
                    CHAIN_ID,
                    block * 1_000 + index as u64,
                )]
            } else {
                vec![]
            };
            writer.append(&header, &signature, &txs).unwrap();
            prev_hash = header.hash();
        }
    }
}

struct Env {
    ctx: ProgramTestContext,
    c: Chain,
    accounts: BanksAccountOps,
    log_dir: tempfile::TempDir,
}

async fn setup() -> Env {
    let settlement_program = Pubkey::new_unique();
    let inbox_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[
            rome_zk_testkit::ProgramSpec::upgradeable("zk_settlement", settlement_program),
            rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox_program),
        ],
        true,
    );
    let payer = funded_keypair();
    pt.add_account(payer.pubkey(), funded_account());
    let c = Chain {
        settlement_program,
        inbox_program,
        authority: funded_keypair(),
        genesis_state_root: keccak::hashv(&[b"genesis"]).to_bytes(),
        registry_authority: funded_keypair(),
        treasury: Pubkey::new_unique(),
        upgrade_authority: funded_keypair(),
    };
    pt.add_account(c.authority.pubkey(), funded_account());
    pt.add_account(
        zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, CHAIN_ID).0,
        cursor_account(inbox_program, CHAIN_ID, 1),
    );
    let mut ctx = pt.start_with_context().await;
    let mut clock: Clock = ctx.banks_client.get_sysvar().await.unwrap();
    clock.unix_timestamp = CLOCK_UNIX_TS;
    ctx.set_sysvar(&clock);
    register_chain(&mut ctx, &payer, &c).await;
    let accounts = BanksAccountOps {
        banks_client: ctx.banks_client.clone(),
    };
    Env {
        ctx,
        c,
        accounts,
        log_dir: tempdir().unwrap(),
    }
}

fn wcfg(c: &Chain) -> WindowConfig {
    WindowConfig {
        inbox_program_id: c.inbox_program,
        settlement_program_id: c.settlement_program,
        payer: c.authority.pubkey(),
        chain_id: CHAIN_ID,
        max_frame_body_len: MAX_FRAME_BODY_LEN,
        chunk_tuning: tuning(),
        open_tuning: tuning(),
        chunk_retry_compute_unit_limit: 1_400_000,
        finalize_tuning: tuning(),
        finalize_poll_interval: Duration::from_millis(1),
        finalize_max_polls: 200,
        in_flight_frames: 8,
        confirm_poll_interval: Duration::from_millis(1),
        signature_status_batch_size: 32,
        batches_in_flight: 2,
        cu_sample: None,
        cu_sample_every: 1,
    }
}

async fn restart(env: &Env, sender: &BanksSender) -> Result<Anchor, StartupError> {
    let w = wcfg(&env.c);
    let cfg = StartupRecover {
        window: &w,
        log_dir: env.log_dir.path(),
        sub_blocks_per_block: SUB_BLOCKS_PER_BLOCK,
        block_gas_limit: BLOCK_GAS_LIMIT,
        blocks_per_batch: 10,
    };
    let metrics = Metrics::new();
    let (sink, _rx) = ChannelPostRootSink::new();
    pipeline::startup_recover(&env.accounts, sender, &metrics, &sink, &cfg).await
}

fn all_blocks(env: &Env) -> Vec<Block> {
    read_blocks(
        env.log_dir.path(),
        &Anchor {
            from_block: 1,
            prev_block_timestamp_secs: 0,
        },
    )
}

fn frames_for(batch: u64, group: &[Block]) -> Vec<Frame> {
    channel::cut_frames(
        CHAIN_ID,
        batch,
        &channel::encode_stream(group),
        MAX_FRAME_BODY_LEN,
    )
}

async fn open(env: &Env, s: &BanksSender, batch: u64, expected_count: u32) {
    let ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &env.c.inbox_program,
        &env.c.authority.pubkey(),
        CHAIN_ID,
        batch,
        expected_count,
        &env.c.settlement_program,
    );
    s.send_and_confirm(&ixs, tuning()).await.unwrap();
}

async fn seal(env: &Env, s: &BanksSender, batch: u64, f: &Frame) {
    let plan = pipeline::plan_chunk(
        &env.c.inbox_program,
        &env.c.authority.pubkey(),
        &env.c.settlement_program,
        CHAIN_ID,
        batch,
        f.frame_no as u32,
        &f.to_bytes(),
    );
    s.send_and_confirm(&plan, tuning()).await.unwrap();
}

async fn batch_acct(env: &Env, batch: u64) -> Option<zk_inbox_client::BatchAccount> {
    read_batch(
        &env.ctx.banks_client,
        &env.c.inbox_program,
        &env.c.settlement_program,
        batch,
    )
    .await
}

fn restarted(env: &Env) -> BanksSender {
    let s = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    s.nonce.store(100_000, Ordering::Relaxed);
    s
}

fn no_abandon(env: &Env, s: &BanksSender) {
    let disc = zk_inbox_client::abandon_batch_ix(
        &env.c.inbox_program,
        &env.c.authority.pubkey(),
        &env.c.settlement_program,
        CHAIN_ID,
        1,
    )
    .data[0];
    assert!(s
        .sent()
        .iter()
        .all(|ix| !(ix.program_id == env.c.inbox_program && ix.data.first() == Some(&disc))));
}

/// An age-closed original group 1..=2 while the log now holds 1..=5.
#[tokio::test]
async fn a_restart_finishes_an_age_closed_group_shorter_than_the_log() {
    let env = setup().await;
    write_log(
        env.log_dir.path(),
        &[(1, 20), (2, 20), (3, 20), (4, 20), (5, 20)],
    );
    let blocks = all_blocks(&env);
    let orig = frames_for(1, &blocks[0..2]);
    eprintln!("age case: original 1..=2 cuts {} frames", orig.len());
    assert!(orig.len() >= 2);
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    open(&env, &first, 1, orig.len() as u32).await;
    seal(&env, &first, 1, &orig[0]).await;
    let s = restarted(&env);
    let anchor = restart(&env, &s).await.expect("recovery");
    let b = batch_acct(&env, 1).await.unwrap();
    assert!(b.finalized);
    pipeline::verify_acc(&b, &orig).expect("finished batch == original channel");
    assert_eq!(anchor.from_block, 3);
    no_abandon(&env, &s);
}

/// No frame landed at all: the batch is finished with a group that starts exactly at the anchor.
#[tokio::test]
async fn a_restart_finishes_a_batch_with_no_leaves_with_a_group_that_starts_at_the_anchor() {
    let env = setup().await;
    write_log(
        env.log_dir.path(),
        &[(1, 20), (2, 20), (3, 20), (4, 20), (5, 20)],
    );
    let blocks = all_blocks(&env);
    let orig = frames_for(1, &blocks[0..3]);
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    open(&env, &first, 1, orig.len() as u32).await;
    let s = restarted(&env);
    let anchor = restart(&env, &s).await.expect("recovery");
    let b = batch_acct(&env, 1).await.unwrap();
    assert!(b.finalized);
    let matched: Vec<usize> = (1..=blocks.len())
        .filter(|&e| pipeline::verify_acc(&b, &frames_for(1, &blocks[0..e])).is_ok())
        .collect();
    eprintln!(
        "zero-leaf: finished group = 1..={matched:?}, anchor {}",
        anchor.from_block
    );
    assert_eq!(matched.len(), 1);
    assert_eq!(anchor.from_block, matched[0] as u64 + 1);
    no_abandon(&env, &s);
}

/// Window of 2: batch 1 zero-leaf (original 1..=2), batch 2 partial (original 3..=5). The smallest end for
/// batch 1 (block 1) gives the same frame count, so the planner must backtrack to end 2.
#[tokio::test]
async fn a_restart_backtracks_over_a_zero_leaf_lower_batch() {
    let env = setup().await;
    write_log(
        env.log_dir.path(),
        &[(1, 1), (2, 1), (3, 20), (4, 20), (5, 20), (6, 20)],
    );
    let blocks = all_blocks(&env);
    let b1 = frames_for(1, &blocks[0..2]);
    let b1_short = frames_for(1, &blocks[0..1]);
    let b2 = frames_for(2, &blocks[2..5]);
    eprintln!(
        "window: b1 {} frames (1..=1 gives {}), b2 {} frames",
        b1.len(),
        b1_short.len(),
        b2.len()
    );
    assert_eq!(b1.len(), b1_short.len(), "fixture must force a backtrack");
    assert!(b2.len() >= 2);
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    open(&env, &first, 1, b1.len() as u32).await;
    open(&env, &first, 2, b2.len() as u32).await;
    seal(&env, &first, 2, &b2[0]).await;
    let s = restarted(&env);
    let anchor = restart(&env, &s).await.expect("recovery");
    let a1 = batch_acct(&env, 1).await.unwrap();
    let a2 = batch_acct(&env, 2).await.unwrap();
    assert!(a1.finalized && a2.finalized);
    pipeline::verify_acc(&a1, &b1).expect("batch 1 == blocks 1..=2");
    pipeline::verify_acc(&a2, &b2).expect("batch 2 == original 3..=5");
    assert_eq!(anchor.from_block, 6);
    no_abandon(&env, &s);
}

/// Frame size changed between crash and restart: ResumeImpossible, nothing sent.
#[tokio::test]
async fn a_frame_size_change_after_a_crash_refuses_and_sends_nothing() {
    let env = setup().await;
    write_log(env.log_dir.path(), &[(1, 20), (2, 20), (3, 20)]);
    let blocks = all_blocks(&env);
    let other = channel::cut_frames(CHAIN_ID, 1, &channel::encode_stream(&blocks), 1_500);
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    open(&env, &first, 1, other.len() as u32).await;
    seal(&env, &first, 1, &other[0]).await;
    let s = restarted(&env);
    let err = restart(&env, &s).await.expect_err("must refuse");
    eprintln!("resume impossible: {err}");
    assert!(matches!(
        err,
        StartupError::Recover(PipelineError::ResumeImpossible {
            batch: 1,
            leaves_present: 1,
            ..
        })
    ));
    assert!(
        s.sent().is_empty(),
        "nothing may be sent: {:?}",
        s.sent().len()
    );
    let b = batch_acct(&env, 1).await.unwrap();
    assert!(!b.finalized && b.leaves_present == 1);
}

/// Every leaf present and finalize already started (finalize_cursor > 0): read back, finish, acc matches.
#[tokio::test]
async fn a_restart_continues_a_finalize_that_had_started() {
    let env = setup().await;
    write_log(env.log_dir.path(), &[(1, 20), (2, 20), (3, 20)]);
    let blocks = all_blocks(&env);
    let frames = frames_for(1, &blocks);
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    open(&env, &first, 1, frames.len() as u32).await;
    for f in &frames {
        seal(&env, &first, 1, f).await;
    }
    let fin = zk_inbox_client::finalize_batch_ix(
        &env.c.inbox_program,
        &env.c.authority.pubkey(),
        &env.c.settlement_program,
        CHAIN_ID,
        1,
        1,
    );
    first
        .send_and_confirm(std::slice::from_ref(&fin), tuning())
        .await
        .unwrap();
    let mid = batch_acct(&env, 1).await.unwrap();
    assert!(!mid.finalized && mid.finalize_cursor == 1);
    let s = restarted(&env);
    let anchor = restart(&env, &s).await.expect("recovery");
    let b = batch_acct(&env, 1).await.unwrap();
    assert!(b.finalized);
    pipeline::verify_acc(&b, &frames).unwrap();
    assert_eq!(anchor.from_block, 4);
    assert!(
        s.sent()
            .iter()
            .all(|ix| ix.program_id != env.c.inbox_program
                || matches!(
                    zk_inbox_client::decode_instruction(&ix.data),
                    Ok(zk_inbox_client::InboxIx::FinalizeBatch { .. })
                )),
        "only FinalizeBatch may be sent when every leaf is present"
    );
}

/// The ordered log has a gap (block 3 missing). Batch 1 = blocks 1..=2 is finalized; batch 2 is open with no
/// leaves. A group starting at block 4 would break the chain's continuity (the live path refuses it with
/// `AnchorError::Gap`), so the resume must refuse: `ResumeImpossible`, nothing sent, batch 2 still open.
#[tokio::test]
async fn a_gap_in_the_log_refuses_to_finish_a_zero_leaf_batch_and_sends_nothing() {
    let env = setup().await;
    write_log(env.log_dir.path(), &[(1, 1), (2, 1), (4, 1), (5, 1)]);
    let blocks = all_blocks(&env);
    assert_eq!(
        blocks.iter().map(|b| b.number).collect::<Vec<_>>(),
        vec![1, 2, 4, 5]
    );
    let first = BanksSender::new(env.ctx.banks_client.clone(), &env.c.authority);
    let b1 = post_group(
        &first,
        &env.accounts,
        env.c.inbox_program,
        env.c.settlement_program,
        env.c.authority.pubkey(),
        &blocks[0..2],
    )
    .await;
    assert_eq!(b1, 1);
    open(&env, &first, 2, 1).await;
    let s = restarted(&env);
    let err = restart(&env, &s).await.expect_err("must refuse");
    assert!(
        matches!(
            err,
            StartupError::Recover(PipelineError::ResumeImpossible {
                batch: 2,
                leaves_present: 0,
                ..
            })
        ),
        "unexpected error: {err}"
    );
    assert!(
        s.sent().is_empty(),
        "nothing may be sent: {:?}",
        s.sent().len()
    );
    let a2 = batch_acct(&env, 2).await.unwrap();
    assert!(
        !a2.finalized && a2.leaves_present == 0,
        "batch 2 must stay open"
    );
}
