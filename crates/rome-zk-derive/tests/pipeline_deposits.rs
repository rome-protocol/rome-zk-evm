//! Deposits at the pipeline level: a v3 batch's deposit range is checked against the records and the
//! blocks' cursor, each block is built with its slice of the deposits as EIP-4895 withdrawals, and any
//! mismatch is `Critical` before the engine is touched. The shape is a small deposit batch: three deposits
//! by three different depositors, credited across three blocks (two in the first, one in the third).

use alloy_primitives::{Address, B256};
use rome_zk_channel as channel;
use rome_zk_channel::Block;
use rome_zk_derive::engine::mock::MockEngineApi;
use rome_zk_derive::engine::EngineController;
use rome_zk_derive::inbox::InboxRetrieval;
use rome_zk_derive::pipeline::{DerivePipeline, StepOutcome};
use rome_zk_derive::testutil::FakeAccountReader;
use rome_zk_derive::traversal::SolanaTraversal;
use rome_zk_derive::PipelineError;
use rome_zk_layouts::batch::{BatchDeposit, BatchFields};
use rome_zk_layouts::deposit::DepositRecord;
use solana_program::pubkey::Pubkey;

const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);
const BRIDGE_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);
const CHAIN_ID: u64 = 200_101;
const OPEN_SLOT: u64 = 1_000;

fn sk() -> fn(&[&[u8]]) -> [u8; 32] {
    rome_zk_merkle::keccak256
}

/// Three deposits: distinct depositors, recipients and amounts (in gwei).
fn records() -> Vec<DepositRecord> {
    (0..3u8)
        .map(|i| DepositRecord {
            sender: [0x10 + i; 32],
            recipient: [0xA0 + i; 20],
            amount_gwei: 1_000_000 * (u64::from(i) + 1),
        })
        .collect()
}

/// The queue's hash chain after each record: element `k` is the value before deposit `from + k`, and the
/// last element is the value before `from + records.len()`.
fn chain_values(from: u64, h_from: [u8; 32], records: &[DepositRecord]) -> Vec<[u8; 32]> {
    let mut out = vec![h_from];
    for k in 0..records.len() {
        out.push(rome_zk_layouts::deposit::chain_through(
            &sk(),
            &SETTLEMENT_PROGRAM.to_bytes(),
            CHAIN_ID,
            from,
            &h_from,
            &records[..=k],
        ));
    }
    out
}

fn block(number: u64, deposits_end: Option<u64>) -> Block {
    Block {
        number,
        timestamp: 1_757_000_000 + number,
        gas_limit: 100_000_000,
        txs: vec![],
        deposits_end,
    }
}

/// Everything one test varies. `Fixture::new(from)` is the honest small batch: records
/// `[from, from + 3)`, blocks ending the cursor at `from + 2`, unchanged, `from + 3`.
struct Fixture {
    from: u64,
    records: Vec<DepositRecord>,
    blocks: Vec<Block>,
    /// The header's range; `None` writes a v2 header.
    range: Option<BatchDeposit>,
    /// Which record accounts exist (index into `records`), and whether exit_config does.
    omit_record: Option<usize>,
    exit_config: bool,
}

impl Fixture {
    fn new(from: u64) -> Self {
        let records = records();
        let h = chain_values(from, [0x5E; 32], &records);
        Self {
            from,
            blocks: vec![
                block(1, Some(from + 2)),
                block(2, None),
                block(3, Some(from + 3)),
            ],
            range: Some(BatchDeposit {
                from,
                to: from + 3,
                hash_from: h[0],
                hash_to: h[3],
            }),
            records,
            omit_record: None,
            exit_config: true,
        }
    }

    /// The honest batch with no deposits: a v3 header with an empty range, or a v2 header.
    fn without_deposits(v3: bool) -> Self {
        let mut f = Self::new(5);
        f.records.clear();
        f.blocks = vec![block(1, None), block(2, None), block(3, None)];
        f.range = v3.then_some(BatchDeposit {
            from: 5,
            to: 5,
            hash_from: [0x5E; 32],
            hash_to: [0x5E; 32],
        });
        f.exit_config = false;
        f
    }

    fn reader(&self, program_id: &Pubkey) -> FakeAccountReader {
        let mut reader = FakeAccountReader::default();

        let compressed = channel::encode_stream(&self.blocks);
        let frames = channel::cut_frames(CHAIN_ID, 0, &compressed, 3_681);
        assert_eq!(frames.len(), 1, "fixture fits in one chunk");
        let frame_bytes = frames[0].to_bytes();

        let chunk_hash = alloy_primitives::keccak256(&frame_bytes).0;
        let empty = BatchDeposit {
            from: 0,
            to: 0,
            hash_from: [0; 32],
            hash_to: [0; 32],
        };
        let (root, forced_root, acc) = zk_inbox_client::reference_commitment_with_deposits(
            CHAIN_ID,
            0,
            OPEN_SLOT,
            &[chunk_hash],
            self.range.as_ref().unwrap_or(&empty),
        );
        let fields = BatchFields {
            chain_id: CHAIN_ID,
            batch: 0,
            open_slot: OPEN_SLOT,
            expected_count: 1,
            leaves_present: 0,
            finalized: true,
            settlement_program: [0u8; 32],
            authority: [0u8; 32],
            root,
            forced_root,
            acc,
            finalize_cursor: 0,
            open_unix_ts: 1_757_000_000,
            deposit: self.range,
        };
        let version = if self.range.is_some() {
            rome_zk_layouts::batch::VERSION_V3
        } else {
            rome_zk_layouts::batch::VERSION
        };
        let mut d = vec![0u8; rome_zk_layouts::batch::account_len_for(version, 1).unwrap()];
        match self.range {
            Some(_) => {
                let header = rome_zk_layouts::batch::write_header_v3(&fields).unwrap();
                d[..header.len()].copy_from_slice(&header);
            }
            None => {
                let header = rome_zk_layouts::batch::write_header(&fields);
                d[..header.len()].copy_from_slice(&header);
            }
        }
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        reader.accounts.insert(batch_pda, d);

        let (chunk_pda, _) =
            zk_inbox_client::chunk_pda(program_id, &SETTLEMENT_PROGRAM, CHAIN_ID, 0, 0);
        let mut chunk =
            rome_zk_layouts::chunk::write_header(&rome_zk_layouts::chunk::ChunkHeaderFields {
                authority: [0u8; 32],
                chain_id: CHAIN_ID,
                batch: 0,
                idx: 0,
                len: frame_bytes.len() as u32,
                sealed: true,
            })
            .to_vec();
        chunk.extend_from_slice(&frame_bytes);
        reader.accounts.insert(chunk_pda, chunk);

        if self.exit_config {
            let cfg = rome_zk_layouts::exit::exit_config::write(
                &rome_zk_layouts::exit::exit_config::ExitConfigFields {
                    chain_id: CHAIN_ID,
                    exit_portal: [0; 20],
                    bridge_program: BRIDGE_PROGRAM.to_bytes(),
                    pending_exit_portal: [0; 20],
                    pending_bridge_program: [0; 32],
                    pending_exit_cap: 0,
                    pending_poster_bond: 0,
                    activation_slot: 0,
                    pending_mask: 0,
                },
            );
            let (pda, _) = rome_zk_layouts::exit::exit_config::pda(&SETTLEMENT_PROGRAM, CHAIN_ID);
            reader.accounts.insert(pda, cfg.to_vec());
        }

        let h = chain_values(self.from, [0x5E; 32], &self.records);
        for (k, r) in self.records.iter().enumerate() {
            if self.omit_record == Some(k) {
                continue;
            }
            let index = self.from + k as u64;
            let mut rec = vec![0u8; rome_zk_layouts::deposit_queue::deposit_record::LEN];
            rome_zk_layouts::deposit_queue::deposit_record::write(
                &mut rec,
                &rome_zk_layouts::deposit_queue::deposit_record::DepositRecordFields {
                    index,
                    enqueue_unix_ts: 1_757_000_000,
                    sender: r.sender,
                    recipient: r.recipient,
                    amount_gwei: r.amount_gwei,
                    hash_after: h[k + 1],
                },
            );
            let (pda, _) = rome_zk_layouts::deposit_queue::deposit_record::pda(
                &BRIDGE_PROGRAM,
                &SETTLEMENT_PROGRAM.to_bytes(),
                CHAIN_ID,
                index,
            );
            reader.accounts.insert(pda, rec);
        }
        reader
    }

    fn pipeline(&self) -> DerivePipeline<FakeAccountReader, MockEngineApi> {
        let program_id = Pubkey::new_unique();
        let reader = self.reader(&program_id);
        let traversal =
            SolanaTraversal::new(reader.clone(), program_id, SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        let inbox = InboxRetrieval::new(reader, program_id, SETTLEMENT_PROGRAM);
        let engine = EngineController::new(MockEngineApi::default(), B256::ZERO, 0);
        DerivePipeline::new(traversal, inbox, engine, CHAIN_ID, Address::ZERO, 16, 10)
    }
}

fn expected_withdrawals(from: u64, recs: &[DepositRecord]) -> Vec<alloy_eips::eip4895::Withdrawal> {
    recs.iter()
        .enumerate()
        .map(|(k, r)| {
            rome_zk_executor_api::deposit_withdrawal(
                from + k as u64,
                Address::from(r.recipient),
                r.amount_gwei,
            )
        })
        .collect()
}

/// The batch is `Critical`, nothing reached the engine, and the cursor stays on the batch.
async fn assert_critical(f: &Fixture, needle: &str) {
    let mut p = f.pipeline();
    let err = p.step().await.unwrap_err();
    match err {
        PipelineError::Critical(msg) => assert!(
            msg.contains(needle),
            "expected a Critical naming {needle:?}, got: {msg}"
        ),
        other => panic!("expected Critical, got {other:?}"),
    }
    assert!(
        p.engine_controller().engine().calls.is_empty(),
        "the engine must never be touched by a batch whose deposits fail"
    );
    assert_eq!(p.next_batch(), 0);
}

#[tokio::test]
async fn a_small_deposit_batch_derives_with_matching_withdrawals_roots() {
    for from in [0u64, 7] {
        let f = Fixture::new(from);
        let mut p = f.pipeline();
        match p.step().await.unwrap() {
            StepOutcome::Derived { batch, blocks } => {
                assert_eq!(batch, 0);
                assert_eq!(blocks.len(), 3);
            }
            StepOutcome::Idle => panic!("expected a derived batch"),
        }
        assert_eq!(p.next_batch(), 1);

        let all = expected_withdrawals(from, &f.records);
        let engine = p.engine_controller().engine();
        // Block 1: deposits from, from + 1. Block 2: none. Block 3: deposit from + 2.
        assert_eq!(
            engine.existing_blocks[&1].withdrawals_root,
            rome_zk_executor_api::withdrawals_root(&all[0..2])
        );
        assert_eq!(
            engine.existing_blocks[&2].withdrawals_root,
            rome_zk_executor_api::EMPTY_WITHDRAWALS
        );
        assert_eq!(
            engine.existing_blocks[&3].withdrawals_root,
            rome_zk_executor_api::withdrawals_root(&all[2..3])
        );
        assert_ne!(
            engine.existing_blocks[&1].withdrawals_root,
            rome_zk_executor_api::EMPTY_WITHDRAWALS
        );
    }
}

#[tokio::test]
async fn a_batch_without_deposits_builds_empty_withdrawals_and_reads_no_deposit_account() {
    // Neither fixture has an exit_config or any record, so a read of either would be a missing account.
    for v3 in [false, true] {
        let f = Fixture::without_deposits(v3);
        let mut p = f.pipeline();
        assert!(matches!(
            p.step().await.unwrap(),
            StepOutcome::Derived { batch: 0, .. }
        ));
        for n in 1..=3u64 {
            assert_eq!(
                p.engine_controller().engine().existing_blocks[&n].withdrawals_root,
                rome_zk_executor_api::EMPTY_WITHDRAWALS
            );
        }
    }
}

#[tokio::test]
async fn a_broken_hash_chain_is_critical() {
    // The header's hash_to is not what the records chain to.
    let mut f = Fixture::new(0);
    f.range.as_mut().unwrap().hash_to = [0xEE; 32];
    assert_critical(&f, "hash chain").await;

    // A record changed after the header was written: the chain no longer ends at hash_to.
    let mut f = Fixture::new(0);
    let honest = f.records.clone();
    f.records[1].amount_gwei += 1;
    // Keep the header over the honest records, put the altered ones in the accounts.
    let h = chain_values(0, [0x5E; 32], &honest);
    f.range = Some(BatchDeposit {
        from: 0,
        to: 3,
        hash_from: h[0],
        hash_to: h[3],
    });
    assert_critical(&f, "hash chain").await;

    // The header's hash_from is not the chain value before the range.
    let mut f = Fixture::new(0);
    f.range.as_mut().unwrap().hash_from = [0xEE; 32];
    assert_critical(&f, "hash chain").await;
}

#[tokio::test]
async fn a_missing_record_is_critical() {
    for k in 0..3 {
        let mut f = Fixture::new(4);
        f.omit_record = Some(k);
        assert_critical(&f, "is missing").await;
    }
}

#[tokio::test]
async fn a_missing_exit_config_is_critical() {
    let mut f = Fixture::new(0);
    f.exit_config = false;
    assert_critical(&f, "exit_config is missing").await;
}

#[tokio::test]
async fn a_cursor_that_does_not_end_at_the_range_end_is_critical() {
    // Ends one short of `to`.
    let mut f = Fixture::new(0);
    f.blocks = vec![block(1, Some(2)), block(2, None), block(3, None)];
    assert_critical(&f, "cursor ends at 2").await;

    // Ends one past `to`.
    let mut f = Fixture::new(0);
    f.blocks = vec![block(1, Some(2)), block(2, None), block(3, Some(4))];
    assert_critical(&f, "cursor ends at 4").await;

    // No block carries a cursor at all although the header names deposits.
    let mut f = Fixture::new(0);
    f.blocks = vec![block(1, None), block(2, None), block(3, None)];
    assert_critical(&f, "cursor ends at 0").await;

    // A cursor that does not rise.
    let mut f = Fixture::new(0);
    f.blocks = vec![block(1, Some(2)), block(2, Some(2)), block(3, Some(3))];
    assert_critical(&f, "deposit cursor is invalid").await;
}

#[tokio::test]
async fn a_fifth_field_in_a_v2_or_empty_range_batch_is_critical() {
    for v3 in [false, true] {
        let mut f = Fixture::without_deposits(v3);
        f.blocks = vec![block(1, None), block(2, Some(6)), block(3, None)];
        assert_critical(&f, "no deposit range").await;
    }
}

#[tokio::test]
async fn an_inverted_range_is_critical() {
    let mut f = Fixture::new(3);
    f.range.as_mut().unwrap().to = 1;
    assert_critical(&f, "inverted").await;
}
