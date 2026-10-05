//! `WindowedPoster`'s bounded posting window — proved against a
//! scripted, in-memory fake chain (fake `Sender` + fake `AccountOps`, no RPC), mirroring the fake-chain
//! pattern this crate's own `anchor.rs`/`resolve.rs` unit tests already use, applied here across BOTH
//! seams together (a single fake models enough of the inbox program's own state machine — `OpenBatch`,
//! chunk `Open`+`Write`+`Seal`+`SealLeaf`, `FinalizeBatch`, `AbandonBatch`, chunk `Close` — to drive
//! `WindowedPoster` and the startup recovery for real, decoding every instruction this
//! crate's own production code actually sends via `zk_inbox_client::decode_instruction`).

use rome_zk_batcher::channel::Block;
use rome_zk_batcher::grouping::SizeCappedGrouper;
use rome_zk_batcher::metrics::Metrics;
use rome_zk_batcher::pipeline::{self, FollowEvent, PipelineError, WindowConfig, WindowedPoster};
use rome_zk_batcher::resolve::{AccountOps, ResolveError};
use rome_zk_batcher::sender::{SendTuning, Sender, SenderError};
use rome_zk_batcher::sink::{FinalizedBatch, PostRootSink};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);
const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);
const BRIDGE_PROGRAM: Pubkey = Pubkey::new_from_array([5u8; 32]);
const CHAIN_ID: u64 = 200_198;

// ===================== the fake chain (Sender + AccountOps over one shared, mutable state) =====================

#[derive(Default, Clone)]
struct FakeBatchState {
    expected_count: u32,
    chunk_bodies: HashMap<u32, Vec<u8>>,
    finalized: bool,
    acc: [u8; 32],
}

#[derive(Default)]
struct Inner {
    next_batch: u64,
    head_final_batch: u64,
    batches: HashMap<u64, FakeBatchState>,
    chunk_pda_index: HashMap<Pubkey, (u64, u32)>,
    batch_pda_index: HashMap<Pubkey, u64>,
    open_order: Vec<u64>,
    chunk_sealed_order: Vec<(u64, u32)>,
    finalize_confirmed_order: Vec<u64>,
    abandon_order: Vec<u64>,
    close_order: Vec<Pubkey>,
    /// Registered by `delay_finalize` — a `FinalizeBatch` targeting this batch blocks on this gate before
    /// this fake ever marks it finalized, so a test can hold it open and observe what happened meanwhile.
    delay_finalize: HashMap<u64, Arc<tokio::sync::Notify>>,
    /// Every `Write` chunk-lane send targeting this batch fails.
    fail_chunk_for: std::collections::HashSet<u64>,
    /// Registered by `delay_cursor_read` — the very next `get_account` read of the
    /// `batch_cursor` PDA blocks on this gate before returning, giving a test deterministic control over
    /// `resolve::resolve_batch_id`'s own await point instead of racing tokio's own scheduling.
    delay_cursor_read: Option<Arc<tokio::sync::Notify>>,
    /// Registered by `delay_chunk_fail_for` — a `Write` targeting this batch (already
    /// in `fail_chunk_for`) blocks on this gate before actually returning its configured failure, so a test
    /// can hold the failure itself open and control exactly when it lands, instead of it firing on the very
    /// first poll of the batch's own settle task.
    delay_chunk_fail: HashMap<u64, Arc<tokio::sync::Notify>>,
    /// Accounts of other programs (the exit config, the deposit queue and its records), each with its owner.
    other_accounts: HashMap<Pubkey, (Vec<u8>, Pubkey)>,
}

#[derive(Clone, Default)]
struct FakeChain(Arc<Mutex<Inner>>);

impl FakeChain {
    /// A chain whose exit config names `BRIDGE_PROGRAM` and whose queue holds one deposit per entry of
    /// `enqueued_at`, each enqueued at that unix time.
    fn seed_deposit_queue(&self, max_per_block: u16, deadline_secs: u32, enqueued_at: &[i64]) {
        use rome_zk_layouts::deposit_queue::{deposit_queue as dq, deposit_record as dr};
        let mut inner = self.0.lock().unwrap();
        let exit_config = rome_zk_layouts::exit::exit_config::write(
            &rome_zk_layouts::exit::exit_config::ExitConfigFields {
                chain_id: CHAIN_ID,
                exit_portal: [7; 20],
                bridge_program: BRIDGE_PROGRAM.to_bytes(),
                pending_exit_portal: [0; 20],
                pending_bridge_program: [0; 32],
                pending_exit_cap: 0,
                pending_poster_bond: 0,
                activation_slot: 0,
                pending_mask: 0,
            },
        );
        inner.other_accounts.insert(
            zk_inbox_client::exit_config_pda(&SETTLEMENT_PROGRAM, CHAIN_ID).0,
            (exit_config.to_vec(), SETTLEMENT_PROGRAM),
        );
        let mut queue = vec![0u8; dq::LEN];
        dq::write(
            &mut queue,
            &dq::DepositQueueFields {
                count: enqueued_at.len() as u64,
                head_hash: [0; 32],
                params: dq::DepositParams {
                    inclusion_deadline_secs: deadline_secs,
                    max_per_batch: 8,
                    max_per_block,
                    min_amount: 1,
                    fee_lamports: 0,
                    fee_recipient: [0; 32],
                },
                pending: dq::DepositParams::default(),
                activation_slot: 0,
            },
        );
        let sp = SETTLEMENT_PROGRAM.to_bytes();
        inner.other_accounts.insert(
            dq::pda(&BRIDGE_PROGRAM, &sp, CHAIN_ID).0,
            (queue, BRIDGE_PROGRAM),
        );
        for (index, ts) in enqueued_at.iter().enumerate() {
            let mut record = vec![0u8; dr::LEN];
            dr::write(
                &mut record,
                &dr::DepositRecordFields {
                    index: index as u64,
                    enqueue_unix_ts: *ts,
                    sender: [1; 32],
                    recipient: [2; 20],
                    amount_gwei: 1,
                    hash_after: [3; 32],
                },
            );
            inner.other_accounts.insert(
                dr::pda(&BRIDGE_PROGRAM, &sp, CHAIN_ID, index as u64).0,
                (record, BRIDGE_PROGRAM),
            );
        }
    }

    /// Puts a pending proposal with `deadline_secs` as its inclusion deadline into the seeded queue.
    fn seed_pending_deadline(&self, deadline_secs: u32) {
        use rome_zk_layouts::deposit_queue::deposit_queue as dq;
        let key = dq::pda(&BRIDGE_PROGRAM, &SETTLEMENT_PROGRAM.to_bytes(), CHAIN_ID).0;
        let mut inner = self.0.lock().unwrap();
        let (data, _) = inner.other_accounts.get_mut(&key).expect("a seeded queue");
        let mut fields = dq::read(data).unwrap();
        fields.pending.inclusion_deadline_secs = deadline_secs;
        fields.activation_slot = 1;
        dq::write(data, &fields);
    }

    fn set_cursor(&self, next_batch: u64) {
        self.0.lock().unwrap().next_batch = next_batch;
    }

    fn set_head_final_batch(&self, head_final_batch: u64) {
        self.0.lock().unwrap().head_final_batch = head_final_batch;
    }

    /// Seeds a pre-existing open-not-finalized batch directly (no sends) — the startup-sweep test's own
    /// "a crashed prior process already left this behind" shape.
    fn seed_open_not_finalized(&self, batch: u64, expected_count: u32) {
        let mut inner = self.0.lock().unwrap();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, batch);
        inner.batch_pda_index.insert(batch_pda, batch);
        for idx in 0..expected_count {
            let (chunk_pda, _) =
                zk_inbox_client::chunk_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, batch, idx);
            inner.chunk_pda_index.insert(chunk_pda, (batch, idx));
        }
        inner.batches.insert(
            batch,
            FakeBatchState {
                expected_count,
                ..Default::default()
            },
        );
    }

    /// Seeds a pre-existing, already-finalized batch directly (nothing to sweep).
    fn seed_finalized(&self, batch: u64) {
        let mut inner = self.0.lock().unwrap();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, batch);
        inner.batch_pda_index.insert(batch_pda, batch);
        inner.batches.insert(
            batch,
            FakeBatchState {
                expected_count: 0,
                finalized: true,
                ..Default::default()
            },
        );
    }

    fn delay_finalize(&self, batch: u64) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        self.0
            .lock()
            .unwrap()
            .delay_finalize
            .insert(batch, gate.clone());
        gate
    }

    /// The next `get_account` read of the `batch_cursor` PDA blocks on the returned
    /// gate before answering — a deterministic stand-in for `resolve::resolve_batch_id`'s own real-RPC
    /// await point, so a test can hold `submit_group` open exactly there and control what happens on
    /// another task meanwhile before releasing it.
    fn delay_cursor_read(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        self.0.lock().unwrap().delay_cursor_read = Some(gate.clone());
        gate
    }

    fn fail_chunk_send_for(&self, batch: u64) {
        self.0.lock().unwrap().fail_chunk_for.insert(batch);
    }

    /// Holds an already-configured `fail_chunk_send_for` failure open until
    /// released — the batch's `Write` blocks on this gate before actually returning the failure, instead
    /// of failing on the very first poll of its own settle task. Lets a test control precisely when a
    /// sibling's failure becomes observable, rather than racing tokio's own task-scheduling order.
    fn delay_chunk_fail_for(&self, batch: u64) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        self.0
            .lock()
            .unwrap()
            .delay_chunk_fail
            .insert(batch, gate.clone());
        gate
    }

    fn open_order(&self) -> Vec<u64> {
        self.0.lock().unwrap().open_order.clone()
    }

    fn finalize_confirmed_order(&self) -> Vec<u64> {
        self.0.lock().unwrap().finalize_confirmed_order.clone()
    }

    fn abandon_order(&self) -> Vec<u64> {
        self.0.lock().unwrap().abandon_order.clone()
    }

    fn batch_exists(&self, batch: u64) -> bool {
        self.0.lock().unwrap().batches.contains_key(&batch)
    }

    fn any_chunk_pda_left_for(&self, batch: u64) -> bool {
        self.0
            .lock()
            .unwrap()
            .chunk_pda_index
            .values()
            .any(|&(b, _)| b == batch)
    }

    fn chunk_sealed_count_for(&self, batch: u64) -> usize {
        self.0
            .lock()
            .unwrap()
            .chunk_sealed_order
            .iter()
            .filter(|(b, _)| *b == batch)
            .count()
    }

    fn encode_batch_account(&self, batch: u64, b: &FakeBatchState) -> Vec<u8> {
        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(
                rome_zk_layouts::batch::VERSION,
                b.expected_count
            )
            .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
            .copy_from_slice(&CHAIN_ID.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
            .copy_from_slice(&batch.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&b.expected_count.to_le_bytes());
        let leaves_present = if b.finalized {
            b.expected_count
        } else {
            b.chunk_bodies.len() as u32
        };
        d[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
            ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
            .copy_from_slice(&leaves_present.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_FINALIZED] = b.finalized as u8;
        d[rome_zk_layouts::batch::OFF_ACC..rome_zk_layouts::batch::OFF_ACC + 32]
            .copy_from_slice(&b.acc);
        // Pre-finalize leaf bytes (`verify_presealed_leaves`'s own format: keccak(frame.to_bytes()) per
        // sealed idx) — irrelevant once finalized (`finalize_and_verify` never reads them then).
        if !b.finalized {
            let leaves_off = rome_zk_layouts::batch::leaves_offset_for(
                rome_zk_layouts::batch::VERSION,
                b.expected_count,
            )
            .unwrap();
            for (&idx, body) in &b.chunk_bodies {
                let hash = solana_program::keccak::hashv(&[body]).to_bytes();
                let slot = leaves_off + 32 * idx as usize;
                let bitmap_off = rome_zk_layouts::batch::HEADER_LEN_V2;
                d[bitmap_off + (idx as usize) / 8] |= 1 << (idx % 8);
                d[slot..slot + 32].copy_from_slice(&hash);
            }
        }
        d
    }

    fn cursor_bytes(&self, next_batch: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
        d[rome_zk_layouts::cursor::OFF_MAGIC..rome_zk_layouts::cursor::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_VERSION] = rome_zk_layouts::cursor::VERSION;
        d[rome_zk_layouts::cursor::OFF_CHAIN_ID..rome_zk_layouts::cursor::OFF_CHAIN_ID + 8]
            .copy_from_slice(&CHAIN_ID.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
            .copy_from_slice(&next_batch.to_le_bytes());
        d
    }

    fn root_bytes(&self, head_final_batch: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::root::MIN_LEN];
        d[rome_zk_layouts::root::OFF_MAGIC..rome_zk_layouts::root::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::root::MAGIC.to_le_bytes());
        d[rome_zk_layouts::root::OFF_CHAIN_ID..rome_zk_layouts::root::OFF_CHAIN_ID + 8]
            .copy_from_slice(&CHAIN_ID.to_le_bytes());
        d[rome_zk_layouts::root::OFF_HEAD_FINAL_BATCH
            ..rome_zk_layouts::root::OFF_HEAD_FINAL_BATCH + 8]
            .copy_from_slice(&head_final_batch.to_le_bytes());
        d
    }
}

impl AccountOps for FakeChain {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        let (root_pda, _) = zk_settlement_client::root_pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
        if let Some((data, _)) = self.0.lock().unwrap().other_accounts.get(pubkey) {
            return Ok(Some(data.clone()));
        }
        if *pubkey == cursor_pda {
            let gate = self.0.lock().unwrap().delay_cursor_read.take();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let next_batch = self.0.lock().unwrap().next_batch;
            return Ok(Some(self.cursor_bytes(next_batch)));
        }
        if *pubkey == root_pda {
            let head_final_batch = self.0.lock().unwrap().head_final_batch;
            return Ok(Some(self.root_bytes(head_final_batch)));
        }
        let inner = self.0.lock().unwrap();
        if let Some(&batch) = inner.batch_pda_index.get(pubkey) {
            return Ok(inner
                .batches
                .get(&batch)
                .map(|b| self.encode_batch_account(batch, b)));
        }
        Ok(None)
    }

    async fn get_account_owner(&self, pubkey: &Pubkey) -> Result<Option<Pubkey>, ResolveError> {
        if let Some((_, owner)) = self.0.lock().unwrap().other_accounts.get(pubkey) {
            return Ok(Some(*owner));
        }
        Ok(self.get_account(pubkey).await?.map(|_| Pubkey::default()))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        let inner = self.0.lock().unwrap();
        Ok(pubkeys
            .iter()
            .map(|p| inner.chunk_pda_index.contains_key(p))
            .collect())
    }
}

fn fake_error() -> SenderError {
    SenderError::ConfirmTimeout {
        resubmits: 0,
        elapsed: Duration::from_secs(0),
    }
}

impl Sender for FakeChain {
    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        _tuning: SendTuning,
    ) -> Result<solana_signature::Signature, SenderError> {
        // Read-only pass: decide whether to fail or block, BEFORE mutating any state.
        let mut finalize_gate = None;
        let mut chunk_fail_gate = None;
        let mut chunk_will_fail = false;
        {
            let inner = self.0.lock().unwrap();
            for ix in instructions {
                if ix.program_id != PROGRAM {
                    continue;
                }
                let Ok(decoded) = zk_inbox_client::decode_instruction(&ix.data) else {
                    continue;
                };
                match decoded {
                    zk_inbox_client::InboxIx::Write { .. } => {
                        let chunk_pda = ix.accounts[1].pubkey;
                        if let Some(&(batch, _)) = inner.chunk_pda_index.get(&chunk_pda) {
                            if inner.fail_chunk_for.contains(&batch) {
                                chunk_will_fail = true;
                                chunk_fail_gate = inner.delay_chunk_fail.get(&batch).cloned();
                            }
                        }
                    }
                    zk_inbox_client::InboxIx::FinalizeBatch { .. }
                    | zk_inbox_client::InboxIx::FinalizeBatchV2 { .. } => {
                        let batch_pda = ix.accounts[0].pubkey;
                        if let Some(&batch) = inner.batch_pda_index.get(&batch_pda) {
                            if let Some(gate) = inner.delay_finalize.get(&batch) {
                                finalize_gate = Some(gate.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(gate) = chunk_fail_gate {
            gate.notified().await;
        }
        if chunk_will_fail {
            return Err(fake_error());
        }
        if let Some(gate) = finalize_gate {
            gate.notified().await;
        }

        let mut inner = self.0.lock().unwrap();
        for ix in instructions {
            if ix.program_id != PROGRAM {
                continue;
            }
            let Ok(decoded) = zk_inbox_client::decode_instruction(&ix.data) else {
                continue;
            };
            match decoded {
                zk_inbox_client::InboxIx::OpenBatch {
                    batch,
                    expected_count,
                    ..
                } => {
                    let (batch_pda, _) =
                        zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, batch);
                    inner.batch_pda_index.insert(batch_pda, batch);
                    for idx in 0..expected_count {
                        let (chunk_pda, _) = zk_inbox_client::chunk_pda(
                            &PROGRAM,
                            &SETTLEMENT_PROGRAM,
                            CHAIN_ID,
                            batch,
                            idx,
                        );
                        inner.chunk_pda_index.insert(chunk_pda, (batch, idx));
                    }
                    inner.batches.insert(
                        batch,
                        FakeBatchState {
                            expected_count,
                            ..Default::default()
                        },
                    );
                    inner.next_batch = batch + 1;
                    inner.open_order.push(batch);
                }
                zk_inbox_client::InboxIx::GrowBatch { .. } => {}
                zk_inbox_client::InboxIx::Write { data, .. } => {
                    let chunk_pda = ix.accounts[1].pubkey;
                    if let Some(&(batch, idx)) = inner.chunk_pda_index.get(&chunk_pda) {
                        inner
                            .batches
                            .get_mut(&batch)
                            .expect("OpenBatch always precedes a chunk Write in this fake's own callers")
                            .chunk_bodies
                            .insert(idx, data);
                    }
                }
                zk_inbox_client::InboxIx::Seal { .. } => {}
                zk_inbox_client::InboxIx::SealLeaf { idx } => {
                    let chunk_pda = ix.accounts[1].pubkey;
                    if let Some(&(batch, _)) = inner.chunk_pda_index.get(&chunk_pda) {
                        inner.chunk_sealed_order.push((batch, idx));
                    }
                }
                zk_inbox_client::InboxIx::FinalizeBatch { .. }
                | zk_inbox_client::InboxIx::FinalizeBatchV2 { .. } => {
                    let batch_pda = ix.accounts[0].pubkey;
                    if let Some(&batch) = inner.batch_pda_index.get(&batch_pda) {
                        if !inner.batches[&batch].finalized {
                            let expected_count = inner.batches[&batch].expected_count;
                            let chunk_hashes: Vec<[u8; 32]> = (0..expected_count)
                                .map(|i| {
                                    solana_program::keccak::hashv(&[
                                        &inner.batches[&batch].chunk_bodies[&i]
                                    ])
                                    .to_bytes()
                                })
                                .collect();
                            let (_, _, acc) = zk_inbox_client::reference_commitment(
                                CHAIN_ID,
                                batch,
                                0,
                                &chunk_hashes,
                            );
                            let b = inner.batches.get_mut(&batch).expect("just read above");
                            b.acc = acc;
                            b.finalized = true;
                        }
                        inner.finalize_confirmed_order.push(batch);
                    }
                }
                zk_inbox_client::InboxIx::AbandonBatch => {
                    let batch_pda = ix.accounts[1].pubkey;
                    if let Some(batch) = inner.batch_pda_index.remove(&batch_pda) {
                        inner.batches.remove(&batch);
                        inner.abandon_order.push(batch);
                    }
                }
                zk_inbox_client::InboxIx::Close => {
                    let chunk_pda = ix.accounts[1].pubkey;
                    inner.chunk_pda_index.remove(&chunk_pda);
                    inner.close_order.push(chunk_pda);
                }
                _ => {}
            }
        }
        Ok(solana_signature::Signature::new_unique())
    }
}

/// Records every `FinalizedBatch` handed off, in order — proves hand-off order (test iii).
#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<u64>>>);

impl PostRootSink for RecordingSink {
    fn publish(&self, batch: FinalizedBatch) {
        self.0.lock().unwrap().push(batch.batch);
    }
}

impl RecordingSink {
    fn order(&self) -> Vec<u64> {
        self.0.lock().unwrap().clone()
    }
}

// ===================== test scaffolding =====================

fn block(number: u64, byte: u8) -> Block {
    Block {
        number,
        timestamp: 1_757_000_000 + number,
        gas_limit: 100_000_000,
        txs: vec![alloy_primitives::Bytes::from(vec![byte; 4])],
        deposits_end: None,
    }
}

fn tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: 200_000,
        loaded_accounts_data_size_limit:
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
        priority_fee_micro_lamports: 1_000,
        max_priority_fee_micro_lamports: 200_000,
        confirm_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

/// The shared `newest_block` counter `WindowedPoster` reads at
/// each batch's own hand-off time — a plain `Arc<AtomicU64>`, exactly what `bin/rome-zk-batcher.rs`'s own
/// main loop hands the poster, seeded here to whatever the test scenario wants the log's own tail to say.
fn shared_newest_block(n: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(n))
}

fn window_config(batches_in_flight: usize) -> WindowConfig {
    WindowConfig {
        inbox_program_id: PROGRAM,
        settlement_program_id: SETTLEMENT_PROGRAM,
        payer: Pubkey::new_unique(),
        chain_id: CHAIN_ID,
        max_frame_body_len: 3_200,
        chunk_tuning: tuning(),
        open_tuning: tuning(),
        chunk_retry_compute_unit_limit: 400_000,
        finalize_tuning: tuning(),
        finalize_poll_interval: Duration::from_millis(1),
        finalize_max_polls: 200,
        in_flight_frames: 16,
        confirm_poll_interval: Duration::from_millis(1),
        signature_status_batch_size: 32,
        batches_in_flight,
        cu_sample: None, // no fake RpcClient transport in this crate's own test doubles.
        // 1 keeps every existing test's behavior exactly as it was before this field
        // existed (every finalized batch is "this batch's turn", and `cu_sample: None` above means
        // nothing observable actually samples) — the cadence tests below build their own `WindowConfig`
        // with a real value via struct-update syntax (`..window_config(n)`).
        cu_sample_every: 1,
    }
}

/// Waits (yielding, never sleeping — deterministic on the current-thread test runtime) until `cond`
/// holds, or panics naming what never happened. Every condition this file waits on is monotonic (a count
/// that only grows, an order vec that only grows), so a bounded number of yields is sufficient — genuinely
/// stuck state is treated as a test failure by design, never a flaky pass.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if cond() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for: {what}");
}

// ===================== bounded window — OpenBatch(N+1) + its chunks before FinalizeBatch(N) =====================

/// Forcing `batches_in_flight` to 1 makes this test fail — `submit_group` for group 1 would then block (waiting for
/// group 0's own settle task to fully complete, including its gated `FinalizeBatch`) before ever sending
/// `OpenBatch(1)`.
#[tokio::test]
async fn open_batch_n_plus_1_and_its_chunks_are_sent_before_finalize_batch_n_confirms() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let gate0 = chain.delay_finalize(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("submitting group 0 (its OpenBatch) must succeed");
    // A `batches_in_flight`-enforcement regression (effectively window=1) would make
    // THIS call block forever — group 0 is gated shut (`delay_finalize(0)`, released only much later in
    // this test) and never completes on its own — hanging the whole test run rather than failing fast.
    // Wrapped in a bounded timeout so that breaking the window fails this test in 5 s, not "CI job never returns".
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        poster.submit_group(vec![block(2, 2)], &mut expected_next_batch),
    )
    .await
    .expect("submit_group(1) must not block waiting for group 0's gated FinalizeBatch — batches_in_flight=2 must let it proceed concurrently")
    .expect("submitting group 1 (its OpenBatch) must succeed — batch 0's FinalizeBatch is gated shut");

    // Both OpenBatch sends have happened (submit_group awaits them synchronously) — group 1's own chunk
    // lane is free to run concurrently with group 0 still stuck on its gated FinalizeBatch.
    assert_eq!(chain.open_order(), vec![0, 1]);
    wait_until("batch 1's chunk sealed", || {
        chain.chunk_sealed_count_for(1) >= 1
    })
    .await;
    assert!(
        chain.finalize_confirmed_order().is_empty(),
        "batch 0's FinalizeBatch must still be gated shut at this point"
    );

    gate0.notify_one();
    poster
        .finish()
        .await
        .expect("both batches must settle cleanly once the gate opens");
    assert_eq!(chain.finalize_confirmed_order(), vec![0, 1]);
}

// ===================== FinalizeBatch(N+1) never before FinalizeBatch(N) confirmed =====================

/// Dropping the finalize-order gate (letting each batch finalize as soon as its own chunks confirm, with no wait on
/// the previous batch) makes this test fail — batch 1's chunks confirm first here (deliberately), so an unordered
/// implementation would finalize 1 before 0.
#[tokio::test]
async fn finalize_batch_n_plus_1_never_precedes_finalize_batch_n_even_when_its_chunks_confirm_first(
) {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let gate0 = chain.delay_finalize(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .unwrap();
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .unwrap();

    // Batch 1's own chunks confirm (no gate on it) well before batch 0's gate ever opens.
    wait_until("batch 1's chunk sealed", || {
        chain.chunk_sealed_count_for(1) >= 1
    })
    .await;
    assert!(chain.finalize_confirmed_order().is_empty());

    gate0.notify_one();
    poster.finish().await.expect("both batches must settle");
    assert_eq!(
        chain.finalize_confirmed_order(),
        vec![0, 1],
        "batch 0's FinalizeBatch must confirm before batch 1's, in id order, regardless of chunk timing"
    );
}

// ===================== hand-off order matches finalize order =====================

/// Handing off on each batch's own completion (instead of gating hand-off behind the SAME chain that orders
/// `FinalizeBatch`) makes this test fail under the same chunk-timing inversion as the FinalizeBatch-order test
/// above (batch 1's chunks confirm first).
#[tokio::test]
async fn the_post_root_sink_receives_batch_n_before_batch_n_plus_1() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let gate0 = chain.delay_finalize(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink.clone()) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .unwrap();
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .unwrap();
    wait_until("batch 1's chunk sealed", || {
        chain.chunk_sealed_count_for(1) >= 1
    })
    .await;

    gate0.notify_one();
    poster.finish().await.expect("both batches must settle");
    assert_eq!(sink.order(), vec![0, 1]);
}

/// The pre-existing regression test in `anchor.rs` pins the OTHER half of this contract (an
/// `OpenNotFinalized` batch found by the RESUME-ANCHOR walk is a caller-ordering bug, never routed
/// around) — named here so a reader of this file finds it.
#[test]
fn see_also_anchor_rs_an_open_not_finalized_batch_in_the_walk_is_a_named_caller_ordering_error() {}

// ===================== a failure anywhere stops the whole run =====================

/// Dropping the `failed` flag check at the top of `submit_group` (relying only on draining order) makes this test
/// fail under an adversarial completion order.
#[tokio::test]
async fn a_failure_in_any_in_flight_batch_stops_the_run_and_sends_nothing_further() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.fail_chunk_send_for(1); // batch 1's own chunk send will fail.
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("group 0 opens and (eventually) finalizes cleanly");
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect(
            "group 1's own OpenBatch still succeeds — only its chunk send is configured to fail",
        );

    // Batch 0 has no gate — it runs to completion on its own; batch 1's chunk send fails. `finish()` must
    // surface that failure and no group 2 is ever attempted (this test never calls `submit_group` a third
    // time — the run's own driving loop, `bin/rome-zk-batcher.rs`, is what stops calling it once an `Err`
    // comes back, exactly as this test's own harness does).
    let err = poster
        .finish()
        .await
        .expect_err("a failed chunk send in any in-flight batch must fail the whole window");
    eprintln!("expected failure surfaced: {err}");
    assert_eq!(
        chain.open_order(),
        vec![0, 1],
        "no OpenBatch(2) — nothing further is posted once a sibling batch failed"
    );
}

/// The same failure, but a THIRD group submitted only after the failure is already guaranteed to have resolved (batch
/// 1's chunk send fails on its very first, synchronous attempt) must itself be refused, never silently opened.
/// Removing BOTH `submit_group`'s eager `drain_ready` call and its `failed`-flag check — leaving only the plain
/// `while len() >= batches_in_flight` wait loop — makes this test fail: that loop's own `await_one` can pick up batch
/// 0's clean, gate-free success FIRST, drop back under the window bound, and let a third `OpenBatch` through without
/// ever having polled batch 1's already-failed future. Either guard alone already closes this for every scenario this
/// crate's own tests construct; both together is defense in depth, not each independently load-bearing here.
#[tokio::test]
async fn submit_group_itself_refuses_once_a_sibling_has_already_failed() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.fail_chunk_send_for(1);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .unwrap();
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .unwrap();
    assert_eq!(chain.open_order(), vec![0, 1]);
    // Give batch 1's spawned settle task a chance to actually run its (immediately-failing, no internal
    // await) chunk send and set `WindowedPoster`'s own shared failure flag before this test tries a third
    // group — there is no external hook to observe that flag directly, so this drives the runtime forward
    // generously via `yield_now` (deterministic on the current-thread test runtime: nothing else races it).
    for _ in 0..10_000 {
        tokio::task::yield_now().await;
    }

    let err = poster
        .submit_group(vec![block(3, 3)], &mut expected_next_batch)
        .await
        .expect_err("a third group must be refused once a sibling already failed");
    eprintln!("submit_group's own refusal: {err}");
    assert_eq!(
        chain.open_order(),
        vec![0, 1],
        "OpenBatch(2) must never be sent once a sibling batch is known to have failed"
    );
}

/// The SAME failure, but this time the race is placed deliberately at the point `submit_group` once had no guard
/// at all — INSIDE `resolve::resolve_batch_id`'s own account reads, after the top-of-function `failed` check
/// already passed (false) and with `batches_in_flight` wide enough (3, against 2 batches actually in flight) that
/// the `while len() >= batches_in_flight` wait loop never runs either — neither of the other two guards is
/// reachable in this interleaving. The gated cursor read (`delay_cursor_read`) makes it deterministic rather than
/// racing tokio's own scheduling: the third `submit_group` blocks there, batch 1's already-configured chunk
/// failure is driven to completion (setting the shared `failed` flag) while it waits, and only then is the cursor
/// read released.
///
/// Removing the re-check of `self.failed` immediately after `resolve_batch_id` returns makes this test fail — with
/// only the top-of-function check and the wait loop, `OpenBatch(2)` would be sent here despite batch 1 having already
/// failed, because neither of those two guards is reachable in this exact interleaving.
#[tokio::test]
async fn submit_group_re_checks_failed_after_resolve_batch_id_even_when_no_wait_loop_runs() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.fail_chunk_send_for(1); // batch 1's own chunk send WILL fail, once its own gate opens below.
    let chunk_gate = chain.delay_chunk_fail_for(1); // ...held shut until this test says otherwise.
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(3), // room for 3 — the wait loop never engages with only 2 batches in flight.
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect(
            "group 0 opens and finalizes cleanly — no gate on it, it plays no role in this race",
        );
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect(
            "group 1's own OpenBatch still succeeds — its chunk send is configured to fail but is held \
             shut on `chunk_gate` for now, so `self.failed` is still false at this point",
        );

    // Let batch 0's own settle task finish first: its finalize reads the cursor too, and the gate below
    // must be taken by the third `submit_group`'s read, not by that one.
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }

    // Gate the NEXT cursor read (the one the third `submit_group`'s own `resolve_batch_id` is about to
    // make) — deterministic control over the exact await point the re-check guards.
    let cursor_gate = chain.delay_cursor_read();

    let handle = tokio::spawn(async move {
        poster
            .submit_group(vec![block(3, 3)], &mut expected_next_batch)
            .await
    });

    // Drive the runtime so the spawned `submit_group` call reaches (and blocks on) the gated cursor read.
    // At this point `self.failed` is GUARANTEED still false — batch 1's own chunk send is separately
    // blocked on `chunk_gate`, which nothing here has released yet.
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }

    // NOW let batch 1 actually fail — its settle task runs to completion and sets the shared `failed`
    // flag — while the third `submit_group` is still parked on the (still-shut) cursor gate, having
    // already passed its own top-of-function check and skipped the wait loop.
    chunk_gate.notify_one();
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }

    // Only now release the cursor read — `resolve_batch_id` resumes and completes; whether `submit_group` refuses
    // depends entirely on its re-check of `self.failed` AFTER this point, which is what this test pins.
    cursor_gate.notify_one();
    let err = handle
        .await
        .expect("the spawned submit_group call must not panic")
        .expect_err(
            "submit_group must refuse once resolve_batch_id returns and finds `failed` now true — no \
             wait loop and no top-of-function check ever caught this interleaving",
        );
    eprintln!("submit_group's own post-resolve_batch_id refusal: {err}");
    assert_eq!(
        chain.open_order(),
        vec![0, 1],
        "OpenBatch(2) must never be sent — batch 1 had already failed by the time resolve_batch_id \
         returned, even though neither the top-of-function check nor the wait loop ever observed it"
    );
}

// ===================== `--follow`'s idle tail-wait observes a failure on its own =====================

/// Making `poll_failure` a no-op (`Ok(())` unconditionally, never draining) makes this test fail.
/// Without it, `--follow`'s own idle tail-wait `tokio::select!` has no way to learn a
/// sibling batch already failed except inside the next `submit_group`/`finish` call — on an idle chain
/// that could be a whole batch period away, or never. `poll_failure` must surface an already-resolved
/// sibling failure on its own, with no further `submit_group` call needed to observe it.
#[tokio::test]
async fn poll_failure_observes_an_already_failed_sibling_without_any_further_submit() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.fail_chunk_send_for(1); // batch 1's own chunk send fails.
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("group 0 opens and finalizes cleanly");
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect(
            "group 1's own OpenBatch still succeeds — only its chunk send is configured to fail",
        );

    // Drive the runtime forward so batch 1's spawned settle task actually runs its (immediately-failing,
    // no internal await) chunk send and stores the shared failure flag — same deterministic-yield idiom
    // `submit_group_itself_refuses_once_a_sibling_has_already_failed` above already uses.
    for _ in 0..10_000 {
        tokio::task::yield_now().await;
    }

    let err = poster.poll_failure().await.expect_err(
        "poll_failure must observe the already-failed sibling on its own — no further submit_group call \
         is needed (or made) to learn this",
    );
    eprintln!("poll_failure's own observed failure: {err}");
    assert_eq!(
        chain.open_order(),
        vec![0, 1],
        "poll_failure must never send anything itself — no OpenBatch(2) here"
    );
}

// ===================== --once behaviour is otherwise unchanged (config default) =====================

/// The default `batches_in_flight` (2) does not change single-batch behavior — one
/// group, submitted and finished, finalizes exactly as it did before the window was added.
#[tokio::test]
async fn a_single_group_still_posts_and_finalizes_normally_under_the_default_window() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    // 2 mirrors `config::default_batches_in_flight` (private to that crate; pinned independently by
    // `config.rs`'s own `default_batches_in_flight_is_two` test) — one group under the default window
    // must behave exactly as a single-batch run always did.
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink.clone()) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .unwrap();
    poster
        .finish()
        .await
        .expect("the single batch must finalize");
    assert_eq!(chain.finalize_confirmed_order(), vec![0]);
    assert_eq!(sink.order(), vec![0]);
    assert_eq!(expected_next_batch, 1);
}

// ===================== cadence metrics observed through the same follow-shaped scenario =====================

/// `batch_post_seconds`' own count equals the number of batches this
/// run finalized, and `lag_blocks` reflects `newest_log_block - last_finalized_block` — both observed
/// through the same scenario as the window tests above (two batches, no gate, no failure).
#[tokio::test]
async fn cadence_metrics_reflect_batches_finalized_and_lag_blocks() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(12),
    );
    let mut expected_next_batch = 0u64;

    // Batch 0 covers blocks 1..=1 (last_block = 1); the shared `newest_block` reads 12 at hand-off time —
    // lag_blocks after batch 0 finalizes must read 12 - 1 = 11.
    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .unwrap();
    // Batch 1 covers blocks 2..=2 (last_block = 2); `newest_block` is still 12 -> lag 10 once IT finalizes
    // (finalizing last, since it's second in id order, so this is the observed final value).
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .unwrap();
    poster.finish().await.expect("both batches must finalize");

    let rendered = metrics.render();
    // The histogram's own `_count` line — exactly 2 observations, one per finalized batch.
    assert!(
        rendered.contains("rome_zk_batcher_batch_post_seconds_count 2"),
        "expected batch_post_seconds_count 2, got:\n{rendered}"
    );
    assert_eq!(
        metrics.lag_blocks.get(),
        10,
        "12 (newest) - 2 (batch 1's last block)"
    );
}

/// `newest_log_block` used to be captured as a plain argument at `submit_group`
/// time — the block that happened to close the group — so the gauge was structurally 0 or 1 no matter how
/// long a batch actually took to finalize. This test **controls the world, not the ruler**: it advances
/// the shared `newest_block` counter AFTER `submit_group` returns but BEFORE the gated batch is allowed to
/// finalize, so a submit-time read and a hand-off-time read observe two genuinely different values — only
/// the hand-off-time read can produce the number this test asserts.
///
/// Capturing `newest_block.load(..)` inside `submit_group` (or anywhere before the `gate0.notify_one()` below)
/// instead of inside `settle_one_batch` at hand-off makes this test fail — it would observe 0 (the value at
/// submission: `newest_block` seeded to `last_block`, unmoved yet), not 7.
#[tokio::test]
async fn lag_blocks_reflects_newest_block_at_hand_off_time_not_at_submit_time() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let gate0 = chain.delay_finalize(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let last_block = 1u64; // block(1, 1)'s own number, the only block in this run's one group.
    let newest_block = shared_newest_block(last_block); // == last_block at submit time: lag would read 0.
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        newest_block.clone(),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("submitting the only group must succeed — its FinalizeBatch is gated shut");

    // The ordered log's own tail keeps advancing while batch 0 sits finalizing (exactly what a live
    // `--follow` run does) — by the time the gate opens, the log is 7 blocks further than it was at submit.
    let advance_by = 7u64;
    newest_block.store(last_block + advance_by, Ordering::Relaxed);

    gate0.notify_one();
    poster
        .finish()
        .await
        .expect("the batch must settle cleanly once the gate opens");

    assert_eq!(
        metrics.lag_blocks.get(),
        advance_by as i64,
        "lag_blocks must be computed from newest_block's value AT HAND-OFF TIME ({} = {} - {}), not the \
         value it held when submit_group returned ({last_block} - {last_block} = 0)",
        advance_by,
        last_block + advance_by,
        last_block,
    );
}

/// Counter-scenario: batch N fails BEFORE it finalizes while batch
/// N+1's chunks are already confirmed. N+1 must NOT finalize — a finalized N+1 beside an abandoned N means
/// N's blocks re-post under a later id at the next start and derive meets block heights out of order. The
/// gate between the two must therefore refuse on a dropped/failed predecessor, never treat "predecessor
/// gone" as "predecessor done".
#[tokio::test]
async fn a_failed_batch_n_never_lets_batch_n_plus_1_finalize() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.fail_chunk_send_for(0); // batch 0 fails at its chunk send; batch 1's chunks succeed.
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics,
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("group 0's OpenBatch succeeds — only its chunk send is configured to fail");
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect("group 1 opens normally under the window");

    let err = poster
        .finish()
        .await
        .expect_err("batch 0's failed chunk send must fail the whole window");
    eprintln!("expected failure surfaced: {err}");
    assert_eq!(
        chain.finalize_confirmed_order(),
        Vec::<u64>::new(),
        "FinalizeBatch(1) must never be sent once batch 0 failed before finalizing — an abandoned 0 beside \
         a finalized 1 re-posts 0's blocks under a later id and derive meets heights out of order"
    );
    assert!(
        chain.batch_exists(1),
        "batch 1 stays open-not-finalized; the next start finishes it"
    );
}

/// Defense in depth: where `FinalizeBatch` is permissionless on chain (a program version from before the
/// authority gate), a third party can finalize N+1 while our N is open-not-finalized — the normal steady
/// state under the window. Finishing N then could not keep the log contiguous, and abandoning it would
/// strand its blocks (a permanent DA hole derive halts on). Startup must refuse by name and
/// send NO `AbandonBatch`, leaving the state repairable (the authority can still finalize N once its
/// leaves are complete). The core fix is the authority-gated `FinalizeBatch` program change.
#[tokio::test]
async fn startup_refuses_and_sends_nothing_when_a_finalized_batch_sits_above_an_open_one() {
    let chain = FakeChain::default();
    let k = 5u64;
    chain.seed_finalized(k - 1);
    chain.seed_open_not_finalized(k, 3);
    chain.seed_finalized(k + 1); // a third party's FinalizeBatch(k+1) landed ahead of our k
    chain.set_cursor(k + 2);
    chain.set_head_final_batch(k - 1);

    let log_dir = tempfile::tempdir().unwrap();
    let cfg = window_config(2);
    let err = pipeline::startup_recover(
        &chain,
        &chain,
        &Metrics::new(),
        &RecordingSink::default(),
        &pipeline::StartupRecover {
            window: &cfg,
            log_dir: log_dir.path(),
            sub_blocks_per_block: 1,
            block_gas_limit: 1_000_000,
            blocks_per_batch: 10,
        },
    )
    .await
    .expect_err("a finalized batch above an open one must be refused, never repaired");
    eprintln!("expected refusal: {err}");
    assert!(
        matches!(
            &err,
            pipeline::StartupError::Recover(PipelineError::FinalizedAboveOpenBatch { open, finalized })
                if *open == k && *finalized == k + 1
        ),
        "the refusal must name both ids, got: {err}"
    );
    assert_eq!(
        chain.abandon_order(),
        Vec::<u64>::new(),
        "no AbandonBatch may be sent — abandoning k here is what strands its blocks"
    );
    assert!(
        chain.batch_exists(k),
        "batch k must stay open-not-finalized, repairable"
    );
    assert!(
        chain.any_chunk_pda_left_for(k),
        "k's chunk PDAs must not be closed"
    );
}

// ===================== cu_sample_every cadence =====================

/// With `cu_sample_every = 3`, only the 3rd, 6th and 9th finalized batch this process ever finalizes are
/// "this batch's turn" — `cu_samples_triggered_total` (bumped in `settle_one_batch` independent of
/// whether a real RPC client is configured, so this crate's own fake-driven harness — which has no fake
/// `RpcClient` transport, see `window_config`'s own doc — can observe the cadence directly) must read
/// exactly 3 after 9 finalized batches, never 9. `batches_in_flight: 1` keeps every batch strictly
/// sequential here (this test's own subject is the modulo cadence, not window overlap — already covered
/// above), so each `submit_group` call fully settles the previous batch before the next opens.
///
/// Removing the `% cfg.cu_sample_every` gate (sampling every batch unconditionally) makes this test fail —
/// `cu_samples_triggered_total` would read 9, not 3.
#[tokio::test]
async fn cu_sample_every_three_triggers_on_the_third_sixth_and_ninth_finalized_batch_only() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        WindowConfig {
            cu_sample_every: 3,
            ..window_config(1)
        },
        shared_newest_block(20),
    );
    let mut expected_next_batch = 0u64;

    for n in 1..=9u64 {
        poster
            .submit_group(vec![block(n, n as u8)], &mut expected_next_batch)
            .await
            .unwrap();
    }
    poster.finish().await.unwrap();

    assert_eq!(
        metrics.batches_finalized_total.get(),
        9,
        "sanity: all 9 batches must have actually finalized"
    );
    assert_eq!(
        metrics.cu_samples_triggered_total.get(),
        3,
        "cu_sample_every=3 over 9 finalized batches must trigger exactly 3 times (batches 3, 6, 9)"
    );
}

/// Sibling: `cu_sample_every = 1` (sample every batch) over the same 9 finalized batches must trigger 9
/// times — proves the gate is a real modulo, not a constant that happens to read 3 above regardless of
/// configuration.
#[tokio::test]
async fn cu_sample_every_one_triggers_on_every_finalized_batch() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        WindowConfig {
            cu_sample_every: 1,
            ..window_config(1)
        },
        shared_newest_block(20),
    );
    let mut expected_next_batch = 0u64;

    for n in 1..=9u64 {
        poster
            .submit_group(vec![block(n, n as u8)], &mut expected_next_batch)
            .await
            .unwrap();
    }
    poster.finish().await.unwrap();

    assert_eq!(metrics.cu_samples_triggered_total.get(), 9);
}

// ===================== `pipeline::follow_tick` / `push_and_post_until_accepted` — the batcher
// closes a partial group by age, on its own receipt clock =====================

/// An idle chain — the grouper stays empty over 100 idle ticks, however far `now` runs
/// ahead — never opens a batch. `close_if_stale`'s own guard (nothing to compare `now` against while
/// empty) makes this true by construction, not merely by luck of the test's own timing.
#[tokio::test]
async fn an_empty_grouper_never_opens_a_batch_over_100_idle_polls() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(0),
    );
    let mut expected_next_batch = 0u64;
    let mut grouper = SizeCappedGrouper::new(120, 900, 3_681, None);
    let close_after = Duration::from_secs(60);
    let t0 = Instant::now();

    for i in 0..100u64 {
        pipeline::follow_tick(
            &mut grouper,
            FollowEvent::Idle,
            t0 + Duration::from_secs(1_000 * i), // far past close_after on every single poll
            close_after,
            &mut expected_next_batch,
            &mut poster,
            &metrics,
        )
        .await
        .expect("an idle tick on an empty grouper never fails");
    }

    assert!(
        chain.open_order().is_empty(),
        "an empty grouper must never open a batch, however far now runs ahead: {:?}",
        chain.open_order()
    );
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["age"])
            .get(),
        0
    );
    assert!(grouper.is_empty());
}

/// A trickle of one block every 2 s, cap 120 (never reached), must still close by age at 60 s — proving `follow_tick`
/// checks `close_if_stale` unconditionally after a `Block` event, not only on the `Idle` arm. A chain sending a block
/// every tick never goes idle at all in this test; an implementation that only checks age in the idle arm would never
/// close this group.
#[tokio::test]
async fn a_trickle_of_one_block_per_2s_with_cap_120_closes_by_age_at_60s() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(0),
    );
    let mut expected_next_batch = 0u64;
    let mut grouper = SizeCappedGrouper::new(120, 900, 3_681, None);
    let close_after = Duration::from_secs(60);
    let t0 = Instant::now();

    // Blocks 0..=30 arrive 2 s apart — block 30 lands at t = 60 s from the first receipt (block 0).
    for i in 0..=30u64 {
        pipeline::follow_tick(
            &mut grouper,
            FollowEvent::Block(block(i, i as u8)),
            t0 + Duration::from_secs(2 * i),
            close_after,
            &mut expected_next_batch,
            &mut poster,
            &metrics,
        )
        .await
        .expect("follow_tick must not fail on an ordinary trickle");
    }

    assert_eq!(
        chain.open_order(),
        vec![0],
        "the trickle must have closed and posted exactly one batch by age, never by cap (only 31 of 120 \
         blocks arrived)"
    );
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["age"])
            .get(),
        1
    );
    assert!(
        grouper.is_empty(),
        "the group must have been taken once it closed by age"
    );
}

/// A partial group sitting at the tail (fewer blocks than `cap`, no more arriving) closes by age once the
/// idle tail-wait's own sleep has elapsed past `close_after` — the shape `--follow`'s idle arm drives every
/// tick once the sequencer stops producing new blocks.
#[tokio::test]
async fn a_partial_group_at_the_tail_closes_by_age_after_the_sleep() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(0),
    );
    let mut expected_next_batch = 0u64;
    let mut grouper = SizeCappedGrouper::new(120, 900, 3_681, None);
    let close_after = Duration::from_secs(60);
    let t0 = Instant::now();

    for i in 0..3u64 {
        pipeline::follow_tick(
            &mut grouper,
            FollowEvent::Block(block(i, i as u8)),
            t0,
            close_after,
            &mut expected_next_batch,
            &mut poster,
            &metrics,
        )
        .await
        .unwrap();
    }
    assert!(
        chain.open_order().is_empty(),
        "not stale yet — nothing posted while the 3 blocks are still fresh"
    );

    // Half-way there the gauge reports the oldest unposted block's real age on the receipt clock.
    pipeline::follow_tick(
        &mut grouper,
        FollowEvent::Idle,
        t0 + Duration::from_secs(30),
        close_after,
        &mut expected_next_batch,
        &mut poster,
        &metrics,
    )
    .await
    .expect("an idle tick before close_after posts nothing");
    assert!(
        chain.open_order().is_empty(),
        "30 s < 60 s: still not stale"
    );
    assert_eq!(
        metrics.oldest_unposted_block_age_seconds.get(),
        30,
        "the gauge is the oldest unposted block's age on the receipt clock, recorded by follow_tick"
    );

    // The log goes quiet: --follow's idle tail-wait ticks with no new block, well past close_after.
    pipeline::follow_tick(
        &mut grouper,
        FollowEvent::Idle,
        t0 + Duration::from_secs(61),
        close_after,
        &mut expected_next_batch,
        &mut poster,
        &metrics,
    )
    .await
    .expect("the idle tick's own age close must succeed");

    assert_eq!(chain.open_order(), vec![0]);
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["age"])
            .get(),
        1
    );
    assert!(grouper.is_empty());
    assert_eq!(
        metrics.oldest_unposted_block_age_seconds.get(),
        0,
        "an empty grouper reports age 0, set by follow_tick after the age close"
    );
}

/// A full group under `follow_tick` closes by `Cap` with the `cap` label — never `age`, even though the
/// first block is 59 s old when the cap fills — and the gauge returns to 0 afterwards. Pins the loop's
/// own label emission and gauge write (the metrics module's render test sets both by hand).
#[tokio::test]
async fn a_full_group_under_follow_tick_closes_with_the_cap_label_and_zeroes_the_gauge() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(0),
    );
    let mut expected_next_batch = 0u64;
    let mut grouper = SizeCappedGrouper::new(60, 900, 3_681, None);
    let close_after = Duration::from_secs(60);
    let t0 = Instant::now();

    for i in 0..60u64 {
        pipeline::follow_tick(
            &mut grouper,
            FollowEvent::Block(block(i, i as u8)),
            t0 + Duration::from_secs(i),
            close_after,
            &mut expected_next_batch,
            &mut poster,
            &metrics,
        )
        .await
        .unwrap();
    }

    assert_eq!(
        chain.open_order(),
        vec![0],
        "block 60 at t0+59 s fills the cap"
    );
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["cap"])
            .get(),
        1,
        "the loop emits the cap label for a cap close"
    );
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["age"])
            .get(),
        0,
        "59 s on the receipt clock is below close_after: never an age close"
    );
    assert!(grouper.is_empty());
    assert_eq!(
        metrics.oldest_unposted_block_age_seconds.get(),
        0,
        "the gauge returns to 0 once the group is posted"
    );
}

/// `--once` drives `push_and_post_until_accepted` directly and
/// never `follow_tick` — a partial group therefore never closes by age here, however far `now` runs ahead,
/// exactly as `--once` behaved before the age close was added (it posts a trailing partial group only once, explicitly,
/// at end-of-log — `bin/rome-zk-batcher.rs`'s own `run_once`, not exercised by this library-level test).
#[tokio::test]
async fn once_mode_behaviour_unchanged() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(0),
    );
    let mut expected_next_batch = 0u64;
    let mut grouper = SizeCappedGrouper::new(120, 900, 3_681, None);

    for i in 0..3u64 {
        // `now` here runs far past any close_after a --follow run would ever use — proving it is simply
        // never consulted on this path.
        pipeline::push_and_post_until_accepted(
            &mut grouper,
            block(i, i as u8),
            t0_plus_hours(i),
            &mut expected_next_batch,
            &mut poster,
            &metrics,
        )
        .await
        .unwrap();
    }

    assert!(
        chain.open_order().is_empty(),
        "--once's own per-block path must never close a partial group by age"
    );
    assert_eq!(
        metrics
            .groups_closed_total
            .with_label_values(&["age"])
            .get(),
        0
    );
    assert_eq!(
        grouper.len(),
        3,
        "the partial group is exactly what --once's own end-of-log path posts"
    );
}

fn t0_plus_hours(i: u64) -> Instant {
    Instant::now() + Duration::from_secs(3_600 * (i + 1))
}

/// The clock-skew histogram is observed by `submit_group` itself, once per opened batch, from the
/// batch account it reads back after `OpenBatch` — pins the production observe site (the unit test
/// in `pipeline.rs` decodes a hand-built account and observes by hand).
#[tokio::test]
async fn submit_group_observes_solana_clock_skew_once_per_opened_batch() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    let metrics = Metrics::new();
    let sink = RecordingSink::default();
    let mut poster = WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        metrics.clone(),
        Arc::new(sink) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    );
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("group 0 posts");
    assert_eq!(
        metrics.solana_clock_skew_seconds.get_sample_count(),
        1,
        "one skew observation per opened batch, taken by submit_group from the read-back account"
    );
    poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect("group 1 posts");
    poster.finish().await.expect("both batches settle");
    assert_eq!(metrics.solana_clock_skew_seconds.get_sample_count(), 2);
}

// ===================== the deposit deadline is checked before OpenBatch =====================

fn wall_clock_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn poster_on(chain: &FakeChain) -> WindowedPoster<FakeChain, FakeChain> {
    WindowedPoster::new(
        Arc::new(chain.clone()),
        Arc::new(chain.clone()),
        Metrics::new(),
        Arc::new(RecordingSink::default()) as Arc<dyn PostRootSink>,
        window_config(2),
        shared_newest_block(10),
    )
}

/// A batch that leaves out a deposit already past its inclusion deadline would be refused at finalize, and
/// could then only be abandoned: it is not opened at all.
#[tokio::test]
async fn a_batch_that_the_inbox_would_refuse_for_an_overdue_deposit_is_not_opened() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    // Two deposits, the first enqueued two hours ago against a one hour deadline.
    chain.seed_deposit_queue(2, 3_600, &[wall_clock_secs() - 7_200, wall_clock_secs()]);
    let mut poster = poster_on(&chain);
    let mut expected_next_batch = 0u64;

    let err = poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect_err("an empty range with deposit 0 overdue must be refused");
    assert!(
        matches!(&err, PipelineError::Deposits(m) if m.contains("was not opened")),
        "unexpected error: {err}"
    );
    assert!(chain.open_order().is_empty(), "no OpenBatch may be sent");
    assert_eq!(expected_next_batch, 0, "the batch id is not used up");
}

/// With the next deposit still inside its window the same group opens.
#[tokio::test]
async fn a_batch_that_leaves_out_only_a_young_deposit_is_opened() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.seed_deposit_queue(2, 3_600, &[wall_clock_secs() - 60, wall_clock_secs()]);
    let mut poster = poster_on(&chain);
    let mut expected_next_batch = 0u64;

    poster
        .submit_group(vec![block(1, 1)], &mut expected_next_batch)
        .await
        .expect("deposit 0 is inside its window");
    assert_eq!(chain.open_order(), vec![0]);
}

/// The second batch of a window starts its range where the first one ends, not at the cursor, which only
/// moves when the first one finalizes: here the first batch takes deposit 0 and the second leaves out deposit 1,
/// which is overdue.
#[tokio::test]
async fn the_next_range_starts_where_the_batch_opened_before_it_ends() {
    let chain = FakeChain::default();
    chain.set_cursor(0);
    chain.set_head_final_batch(0);
    chain.seed_deposit_queue(
        1,
        3_600,
        &[wall_clock_secs() - 60, wall_clock_secs() - 7_200],
    );
    let mut poster = poster_on(&chain);
    let mut expected_next_batch = 0u64;

    let mut taking_the_first = block(1, 1);
    taking_the_first.deposits_end = Some(1);
    poster
        .submit_group(vec![taking_the_first], &mut expected_next_batch)
        .await
        .expect("a full block of one deposit may leave an overdue one out");
    let err = poster
        .submit_group(vec![block(2, 2)], &mut expected_next_batch)
        .await
        .expect_err("the second batch starts at deposit 1, which is overdue");
    assert!(
        matches!(&err, PipelineError::Deposits(m) if m.contains("was not opened")),
        "unexpected error: {err}"
    );
    assert_eq!(chain.open_order(), vec![0]);
}

/// What `check_deadline_at_open` says about a deposit enqueued at 1_000_000 that the batch leaves out, with the
/// batch about to open `seconds_after_enqueue` later.
async fn check_open(chain: &FakeChain, seconds_after_enqueue: i64) -> Result<(), PipelineError> {
    rome_zk_batcher::deposits::check_deadline_at_open(
        chain,
        &SETTLEMENT_PROGRAM,
        CHAIN_ID,
        0,
        0,
        0,
        1_000_000 + seconds_after_enqueue,
    )
    .await
}

/// The host clock and the cluster clock differ and `OpenBatch` lands after the check, so a batch is not opened
/// within 300 s of the deposit's deadline.
#[tokio::test]
async fn a_batch_is_not_opened_within_300_seconds_of_the_deadline() {
    let chain = FakeChain::default();
    chain.seed_deposit_queue(2, 3_600, &[1_000_000, 1_000_000]);
    // 301 s before the deadline: opens.
    check_open(&chain, 3_600 - 301)
        .await
        .expect("301 s before the deadline is open-able");
    // 300 s before: refused (age >= deadline - 300).
    let err = check_open(&chain, 3_600 - 300)
        .await
        .expect_err("300 s before the deadline is refused");
    assert!(
        matches!(&err, PipelineError::Deposits(m) if m.contains("was not opened")),
        "unexpected error: {err}"
    );
    // 299 s before: refused.
    let err = check_open(&chain, 3_600 - 299)
        .await
        .expect_err("299 s before the deadline is refused");
    assert!(
        matches!(&err, PipelineError::Deposits(m) if m.contains("was not opened")),
        "unexpected error: {err}"
    );
}

/// While a proposal waits to activate, the shorter of the two inclusion deadlines applies.
#[tokio::test]
async fn the_shorter_of_the_active_and_pending_deadline_is_the_one_applied() {
    // The pending deadline is shorter: it is the one used.
    let chain = FakeChain::default();
    chain.seed_deposit_queue(2, 3_600, &[1_000_000, 1_000_000]);
    chain.seed_pending_deadline(1_800);
    check_open(&chain, 1_800 - 301)
        .await
        .expect("301 s before the pending deadline opens");
    check_open(&chain, 1_800 - 299)
        .await
        .expect_err("299 s before the pending deadline is refused");
    // The active deadline is shorter: it is the one used, and a longer pending one does not relax it.
    let chain = FakeChain::default();
    chain.seed_deposit_queue(2, 1_800, &[1_000_000, 1_000_000]);
    chain.seed_pending_deadline(3_600);
    check_open(&chain, 1_800 - 301)
        .await
        .expect("301 s before the active deadline opens");
    check_open(&chain, 1_800 - 299)
        .await
        .expect_err("299 s before the active deadline is refused");
}
