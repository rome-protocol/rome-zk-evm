//! `solana-program-test` dry run of the `--once <log_dir>` flow the `rome-zk-batcher` binary drives:
//! write a real ordered log via `rome_zk_sequencer::log::LogWriter` -> `BlockSource` groups it into
//! blocks (exactly what `--once` does) -> `preflight::check` passes against a program-test root account
//! -> `resume::decide` reads `Missing` for a batch id that has never been opened -> `OpenBatch` -> the
//! same per-frame `{Open(+Write)+Seal}` + `SealLeaf` -> `FinalizeBatch` -> `acc` on chain ==
//! `zk_inbox_client::reference_commitment(...)`.
//!
//! Uses `BanksClient` to execute instructions (not `RpcSender`/RPC — see `tests/pipeline.rs`'s module doc
//! for why: `solana-program-test` has no `RpcClient`-compatible transport). This proves every piece the
//! binary wires together (log -> `BlockSource` -> `pipeline` -> `preflight`/`resume`) actually integrates,
//! short of the live network transport itself (exercised instead by the devnet measurement).

use alloy::primitives::B256;
use alloy::signers::local::PrivateKeySigner;
use rome_zk_batcher::anchor;
use rome_zk_batcher::channel::Block;
use rome_zk_batcher::grouping::{PushOutcome, SizeCappedGrouper};
use rome_zk_batcher::pipeline::{self, BatchTarget};
use rome_zk_batcher::preflight;
use rome_zk_batcher::resolve::{self, AccountOps, ResolveError, ResolveOutcome};
use rome_zk_batcher::resume::{self, BatchAccountState, ResumeAction};
use rome_zk_batcher::source::BlockSource;
use rome_zk_sequencer::header::SubBlockHeader;
use rome_zk_sequencer::log::LogWriter;
use rome_zk_sequencer::sealer::SUB_BLOCKS_PER_BLOCK;
use rome_zk_sequencer::signing::sign_header;
use rome_zk_sequencer::testutil::signed_raw_tx;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use tempfile::tempdir;

const CHAIN_ID: u64 = 200_198;

/// Bridges [`AccountOps`] straight to a real `BanksClient` — no JSON-RPC faking needed.
/// `resolve_batch_id`, `anchor::resolve_anchor` and `pipeline::finalize_and_verify`
/// are all generic over [`AccountOps`] now, so this is the whole bridge any of them need; unlike
/// `finalize_cu_limit.rs`'s own `BanksAccountRpc`, which stays a JSON-RPC-shaped bridge for a different
/// reason — measuring real, BPF-metered CU through an actual `RpcClient::new_sender`-wrapped live bank.
#[derive(Clone)]
struct BanksAccountOps {
    banks_client: solana_program_test::BanksClient,
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

/// Thin adapter over `rome_zk_testkit::send_measuring_cu` — this file never needs the CU figure.
async fn send(
    ctx: &mut solana_program_test::ProgramTestContext,
    ixs: &[solana_program::instruction::Instruction],
    payer: &Keypair,
) -> Result<(), TransactionError> {
    rome_zk_testkit::send_measuring_cu(ctx, ixs, payer, &[])
        .await
        .0
}

/// Writes a real ordered log (2 complete blocks, `SUB_BLOCKS_PER_BLOCK` sub-blocks each, one signed tx
/// per sub-block) to a temp directory — exactly the shape `--once <log_dir>` reads.
fn write_two_block_log(dir: &std::path::Path) {
    let sender = PrivateKeySigner::random();
    let mut writer = LogWriter::open(dir, 10_000).unwrap();
    let mut prev_hash = B256::ZERO;
    let mut nonce = 0u64;
    // The ordered log starts at block 1 — genesis 0 is never sealed.
    for block in 1..=2u64 {
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
            let tx = signed_raw_tx(&sender, CHAIN_ID, nonce);
            nonce += 1;
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }
    }
}

/// Appends `count` more blocks, starting at `start_block`, to whatever log already lives at `dir`
/// (`LogWriter::open` resumes appending — `rome_zk_sequencer::log`'s own doc) — what a rerun's grown log
/// looks like.
fn write_blocks_from(
    dir: &std::path::Path,
    sender: &PrivateKeySigner,
    start_block: u64,
    count: u64,
    mut prev_hash: B256,
) -> B256 {
    let mut writer = LogWriter::open(dir, 10_000).unwrap();
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
            let tx = signed_raw_tx(sender, CHAIN_ID, block * 1_000 + index as u64);
            writer.append(&header, &signature, &[tx]).unwrap();
            prev_hash = header.hash();
        }
    }
    prev_hash
}

/// The one grouping path `--once`/`--follow` both drive from the chain anchor —
/// `SizeCappedGrouper` seeded with `anchor.from_block.checked_sub(1)`, posting (here: just collecting)
/// every group that closes, then the final partial group at end-of-input. `cap` is generous on
/// frames/bytes so only the block-count cap ever binds, matching this file's tests (none of which need the
/// size close).
fn group_from_anchor(blocks: Vec<Block>, cap: u64, seed: Option<u64>) -> Vec<Vec<Block>> {
    let mut grouper = SizeCappedGrouper::new(cap, 100_000, 10_000_000, seed);
    let mut groups = Vec::new();
    let now = std::time::Instant::now();
    for mut block in blocks {
        loop {
            match grouper
                .push(block.clone(), now)
                .expect("test data is contiguous")
            {
                PushOutcome::Accepted => break,
                PushOutcome::Closed { carry_over, .. } => {
                    groups.push(grouper.take_group());
                    match carry_over {
                        Some(carried) => {
                            block = carried;
                            continue;
                        }
                        None => break,
                    }
                }
            }
        }
    }
    if !grouper.is_empty() {
        groups.push(grouper.take_group());
    }
    groups
}

/// Posts one group end to end against the real program (the same steps `bin/rome-zk-batcher.rs`'s
/// `post_one_group` drives): re-derive-before-send, `resolve_batch_id` (its own
/// `expected_next_batch` guard included) -> `OpenBatch`(+`GrowBatch`) -> every frame's chunk lane ->
/// `FinalizeBatch` -> verify `acc`. Advances `*expected_next_batch` exactly as the binary does — only on a
/// genuine new post, never on `AlreadyPosted`.
#[allow(clippy::too_many_arguments)]
async fn post_group_real(
    ctx: &mut solana_program_test::ProgramTestContext,
    accounts: &BanksAccountOps,
    program_id: Pubkey,
    settlement_program: Pubkey,
    payer_kp: &Keypair,
    chain_id: u64,
    group: &[Block],
    expected_next_batch: &mut u64,
) -> u64 {
    let compressed = rome_zk_batcher::channel::encode_stream(group);
    pipeline::re_derive_and_check(group, &compressed).unwrap();

    let batch = match resolve::resolve_batch_id(
        accounts,
        &program_id,
        &settlement_program,
        chain_id,
        &compressed,
        rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN,
        *expected_next_batch,
    )
    .await
    .unwrap()
    {
        ResolveOutcome::PostUnder(b) => b,
        ResolveOutcome::AlreadyPosted(b) => return b,
    };

    let frames = rome_zk_batcher::channel::cut_frames(
        chain_id,
        batch,
        &compressed,
        rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN,
    );
    assert!(!frames.is_empty());

    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &payer_kp.pubkey(),
        chain_id,
        batch,
        frames.len() as u32,
        &settlement_program,
    );
    send(ctx, &open_and_grow_ixs, payer_kp).await.unwrap();

    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: payer_kp.pubkey(),
        chain_id,
        batch,
    };
    let frame_jobs = pipeline::build_frame_jobs(target, &frames);
    for stages in &frame_jobs {
        for stage in stages {
            for tx in stage {
                send(ctx, tx, payer_kp).await.unwrap();
            }
        }
    }

    let finalize_ix = zk_inbox_client::finalize_batch_ix(
        &program_id,
        &payer_kp.pubkey(),
        &settlement_program,
        chain_id,
        batch,
        0,
    );
    send(ctx, &[finalize_ix], payer_kp).await.unwrap();

    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, chain_id, batch);
    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account must exist after OpenBatch");
    let decoded = zk_inbox_client::decode_batch_account(&account.data).unwrap();
    assert!(decoded.finalized, "batch {batch} must be finalized");
    pipeline::verify_acc(&decoded, &frames)
        .unwrap_or_else(|e| panic!("batch {batch}'s on-chain acc must match: {e}"));

    *expected_next_batch += 1;
    batch
}

/// `--once` must post at most `blocks_per_batch` consecutive blocks per batch id,
/// looping over groups, instead of everything in one batch. Drives the real on-chain `batch_cursor`
/// program logic across *two* sequential `OpenBatch`es in one run, resolving each group's batch id via
/// the real `resolve::resolve_batch_id` (through [`BanksAccountOps`], not a bypassed loop index) —
/// proving the second group's `OpenBatch` succeeds only because the first group's `OpenBatch` really did
/// advance the on-chain cursor, and that `resolve_batch_id` itself reads that advance back correctly.
#[tokio::test]
async fn once_cap_posts_three_blocks_as_two_batches_of_two_and_one() {
    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let payer_kp = Keypair::new();
    pt.add_account(
        payer_kp.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        zk_inbox_client::root_pda(&settlement_program, CHAIN_ID).0,
        root_account_with_authority(CHAIN_ID, &payer_kp.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0,
        cursor_account(program_id, CHAIN_ID, 0),
    );
    let mut ctx = pt.start_with_context().await;

    let log_dir = tempdir().unwrap();
    let sender = PrivateKeySigner::random();
    let last_hash = write_blocks_from(log_dir.path(), &sender, 1, 3, B256::ZERO);

    let accounts = BanksAccountOps {
        banks_client: ctx.banks_client.clone(),
    };

    // --once's one grouping path, from the anchor (genesis: from_block=0 here).
    let anchor = anchor::resolve_anchor(
        &accounts,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        log_dir.path(),
        SUB_BLOCKS_PER_BLOCK,
        100_000_000,
    )
    .await
    .unwrap();
    assert_eq!(anchor.from_block, 1);

    let mut source = BlockSource::open(
        log_dir.path(),
        CHAIN_ID,
        100_000_000,
        SUB_BLOCKS_PER_BLOCK,
        anchor.from_block,
        anchor.prev_block_timestamp_secs,
    )
    .unwrap();
    let mut blocks = Vec::new();
    while let Some(sourced) = source.next_block().unwrap() {
        blocks.push(sourced.block);
    }
    assert_eq!(blocks.len(), 3, "the log holds exactly 3 complete blocks");
    anchor::verify_first_block_matches(&anchor, blocks[0].number).unwrap();

    let groups = group_from_anchor(blocks, 2, anchor.from_block.checked_sub(1));
    assert_eq!(groups.len(), 2, "3 blocks at cap 2 -> groups of 2 and 1");
    assert_eq!(groups[0].len(), 2);
    assert_eq!(groups[1].len(), 1);

    // This run's own expected_next_batch, read once, right where the binary reads it
    // (after the startup abandon — nothing to abandon on a fresh chain).
    let mut expected_next_batch =
        resolve::read_cursor_next_batch(&accounts, &program_id, &settlement_program, CHAIN_ID)
            .await
            .unwrap();
    assert_eq!(expected_next_batch, 0);

    for (expected_batch, group) in groups.iter().enumerate() {
        let expected_batch = expected_batch as u64;
        // Resolved through the real `resolve_batch_id` (not a bypassed loop index) — proves the previous
        // group's OpenBatch really advanced the on-chain cursor and this run reads that advance back.
        let batch = post_group_real(
            &mut ctx,
            &accounts,
            program_id,
            settlement_program,
            &payer_kp,
            CHAIN_ID,
            group,
            &mut expected_next_batch,
        )
        .await;
        assert_eq!(
            batch, expected_batch,
            "resolve_batch_id must resolve to exactly this group's expected batch id"
        );
    }
    assert_eq!(expected_next_batch, 2);

    // Both groups' worth of blocks (3 total) are accounted for across exactly 2 batch ids — nothing was
    // silently merged into one oversized batch, and nothing was dropped.
    let (cursor_pda, _) = zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID);
    let cursor_data = ctx
        .banks_client
        .get_account(cursor_pda)
        .await
        .unwrap()
        .unwrap();
    let cursor = zk_inbox_client::decode_batch_cursor(&cursor_data.data).unwrap();
    assert_eq!(cursor.next_batch, 2, "exactly two batches were opened");

    // ===== A rerun of `--once` after the log has
    // GROWN past a partial tail group (the first run's last batch closed at 1 block, not a multiple of the
    // cap) must post exactly the new blocks — [4, 5] as one new batch — through the real anchor (now
    // resolved from the real, finalized batch 1's own chunks) and the real one-grouping-path, decodable by
    // `rome-zk-derive`'s own `decode_batch` with `expected_first_block = Some(4)`. This is the exact shape
    // an earlier `--once` (regroup from block 0, `drop_already_posted_groups`) got wrong: 3 blocks at cap
    // 2 regrouped as [0,1]/[2], but a rerun over 5 blocks at the same cap regroups as [0,1]/[2,3]/[4] —
    // group 2 (`[2,3]`) does not start at the anchor (3), a false gap. =====
    write_blocks_from(log_dir.path(), &sender, 4, 2, last_hash);

    let anchor = anchor::resolve_anchor(
        &accounts,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        log_dir.path(),
        SUB_BLOCKS_PER_BLOCK,
        100_000_000,
    )
    .await
    .expect("resolve_anchor must decode the real, finalized batch 1's own chunks");
    assert_eq!(
        anchor.from_block, 4,
        "the anchor must be exactly one past the last block (3) the first run actually posted"
    );

    let mut rerun_source = BlockSource::open(
        log_dir.path(),
        CHAIN_ID,
        100_000_000,
        SUB_BLOCKS_PER_BLOCK,
        anchor.from_block,
        anchor.prev_block_timestamp_secs,
    )
    .unwrap();
    let mut rerun_blocks = Vec::new();
    while let Some(sourced) = rerun_source.next_block().unwrap() {
        rerun_blocks.push(sourced.block);
    }
    assert_eq!(
        rerun_blocks.iter().map(|b| b.number).collect::<Vec<_>>(),
        vec![4, 5],
        "the log's first block read after the grown-log rerun must be exactly the anchor's own block"
    );
    anchor::verify_first_block_matches(&anchor, rerun_blocks[0].number).unwrap();

    let rerun_groups = group_from_anchor(rerun_blocks, 2, anchor.from_block.checked_sub(1));
    assert_eq!(
        rerun_groups.len(),
        1,
        "the grown log's only new group is [3, 4] — one group, not a false gap"
    );
    assert_eq!(rerun_groups[0].len(), 2);

    let rerun_batch = post_group_real(
        &mut ctx,
        &accounts,
        program_id,
        settlement_program,
        &payer_kp,
        CHAIN_ID,
        &rerun_groups[0],
        &mut expected_next_batch,
    )
    .await;
    assert_eq!(
        rerun_batch, 2,
        "the grown-log rerun posts under a fresh batch id, batch 2"
    );
    assert_eq!(expected_next_batch, 3);

    // rome-zk-derive's own decode_batch must accept the rerun's batch with the right continuity threaded
    // from the first run's last batch (0-indexed batch 1 ended at block 3 -> expected_first_block = 4).
    let compressed = rome_zk_batcher::channel::encode_stream(&rerun_groups[0]);
    let decoded = rome_zk_derive::batch_queue::decode_batch(&compressed, CHAIN_ID, 2, 2, Some(4))
        .expect("derive must accept the grown-log rerun's batch with continuity from block 4");
    assert_eq!(decoded, rerun_groups[0]);

    // A further rerun over the now-unchanged (again) log posts nothing new.
    let anchor_after = anchor::resolve_anchor(
        &accounts,
        &program_id,
        &settlement_program,
        CHAIN_ID,
        log_dir.path(),
        SUB_BLOCKS_PER_BLOCK,
        100_000_000,
    )
    .await
    .unwrap();
    assert_eq!(anchor_after.from_block, 6);
    let mut final_source = BlockSource::open(
        log_dir.path(),
        CHAIN_ID,
        100_000_000,
        SUB_BLOCKS_PER_BLOCK,
        anchor_after.from_block,
        anchor_after.prev_block_timestamp_secs,
    )
    .unwrap();
    assert!(
        final_source.next_block().unwrap().is_none(),
        "a rerun over an unchanged log must find nothing past the anchor"
    );
}

#[tokio::test]
async fn once_dry_run_reads_a_real_log_preflights_opens_sends_finalizes_and_verifies() {
    let program_id = Pubkey::new_unique();
    let settlement_program = Pubkey::new_unique();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let payer_kp = Keypair::new();
    pt.add_account(
        payer_kp.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        zk_inbox_client::root_pda(&settlement_program, CHAIN_ID).0,
        root_account_with_authority(CHAIN_ID, &payer_kp.pubkey(), settlement_program),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&program_id, &settlement_program, CHAIN_ID).0,
        cursor_account(program_id, CHAIN_ID, 0),
    );
    let mut ctx = pt.start_with_context().await;

    // --- step 1: `--once <log_dir>` reads a real ordered log via `BlockSource` ---
    let log_dir = tempdir().unwrap();
    write_two_block_log(log_dir.path());
    let mut source = BlockSource::open(
        log_dir.path(),
        CHAIN_ID,
        100_000_000,
        SUB_BLOCKS_PER_BLOCK,
        1,
        0,
    )
    .unwrap();
    let mut blocks = Vec::new();
    while let Some(sourced) = source.next_block().unwrap() {
        blocks.push(sourced.block);
    }
    assert_eq!(blocks.len(), 2, "the log holds exactly 2 complete blocks");

    // --- step 2: preflight — the pure check the binary runs before spending anything ---
    let root_view = zk_settlement_client::RootAccount {
        chain_id: CHAIN_ID,
        number: 0,
        parent_hash: [0; 32],
        state_root: [0; 32],
        block_hash: [0; 32],
        updates: 0,
        profile: 0,
        challenge_window_slots: 0,
        prove_window_slots: 0,
        proving_policy: 0,
        poster_bond: 0,
        exit_cap_per_window: 0,
        authority: payer_kp.pubkey(),
        head_pending_batch: 0,
        head_final_batch: 0,
        pending_count: 0,
        max_pending: 0,
    };
    let registry_view = zk_settlement_client::RegistryAccount {
        chain_id: CHAIN_ID,
        inbox_program: program_id,
        count: 1,
        entries: vec![],
    };
    preflight::check(&root_view, &registry_view, &payer_kp.pubkey(), &program_id)
        .expect("preflight must pass: payer is the root authority, inbox program matches");

    // --- step 3: resume — batch 0 has never been opened, so the decision is PostFresh ---
    let batch = 0u64;
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, batch);
    assert!(
        ctx.banks_client
            .get_account(batch_pda)
            .await
            .unwrap()
            .is_none(),
        "batch account must not exist yet"
    );
    assert_eq!(
        resume::decide(BatchAccountState::Missing, false),
        ResumeAction::PostFresh
    );

    // --- step 4: re-derive-before-send, cut frames, OpenBatch ---
    let compressed = rome_zk_batcher::channel::encode_stream(&blocks);
    pipeline::re_derive_and_check(&blocks, &compressed).unwrap();
    let frames = rome_zk_batcher::channel::cut_frames(
        CHAIN_ID,
        batch,
        &compressed,
        rome_zk_batcher::channel::DEFAULT_MAX_FRAME_BODY_LEN,
    );
    assert!(!frames.is_empty());

    let open_and_grow_ixs = zk_inbox_client::open_and_grow_batch_ixs(
        &program_id,
        &payer_kp.pubkey(),
        CHAIN_ID,
        batch,
        frames.len() as u32,
        &settlement_program,
    );
    send(&mut ctx, &open_and_grow_ixs, &payer_kp).await.unwrap();

    // --- step 5: every frame's {Open(+Write)+Seal} + SealLeaf (same instruction plan the binary's
    // batched sender submits — driven here via BanksClient) ---
    let target = BatchTarget {
        program_id,
        settlement_program,
        payer: payer_kp.pubkey(),
        chain_id: CHAIN_ID,
        batch,
    };
    // One entry per frame, each an explicit stage DAG — sending every stage's transaction(s) in stage
    // order (never a later stage before an earlier one) matches what `RpcSender::send_and_confirm_many`
    // enforces on real devnet; within one stage, sequential-forward here is also correct (the program
    // does not care about within-stage order, only that a stage is done before the next is attempted).
    let frame_jobs = pipeline::build_frame_jobs(target, &frames);
    for stages in &frame_jobs {
        for stage in stages {
            for tx in stage {
                send(&mut ctx, tx, &payer_kp).await.unwrap();
            }
        }
    }

    // --- step 6: FinalizeBatch, then verify acc against the client-side reference ---
    let finalize_ix = zk_inbox_client::finalize_batch_ix(
        &program_id,
        &payer_kp.pubkey(),
        &settlement_program,
        CHAIN_ID,
        batch,
        0,
    );
    send(&mut ctx, &[finalize_ix], &payer_kp).await.unwrap();

    let account = ctx
        .banks_client
        .get_account(batch_pda)
        .await
        .unwrap()
        .expect("batch account must exist after OpenBatch");
    let decoded = zk_inbox_client::decode_batch_account(&account.data).unwrap();
    assert!(
        decoded.finalized,
        "FinalizeBatch must have finalized the batch"
    );
    pipeline::verify_acc(&decoded, &frames)
        .expect("on-chain acc must match the client-side reference_commitment");

    // Batch id 1 (not yet opened) must independently read back as Missing/PostFresh — the next `--once`
    // run's resume scan would land here.
    let (next_batch_pda, _) =
        zk_inbox_client::batch_pda(&program_id, &settlement_program, CHAIN_ID, batch + 1);
    assert!(ctx
        .banks_client
        .get_account(next_batch_pda)
        .await
        .unwrap()
        .is_none());
}
