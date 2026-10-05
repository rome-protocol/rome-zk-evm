//! `solana-program-test` tests for `FinalizeBatchV2 { step, deposit_to }` and header v3.
//!
//! Loads the real `cargo build-sbf` `.so`, so the CU figures printed are real BPF numbers - run
//! `cargo build-sbf --arch v3 --manifest-path programs/zk-inbox/Cargo.toml` first. The queue, the exit config
//! and the deposit records belong to other programs, so they are written as fixture accounts, as the bridge
//! tests do; only the inbox runs.

use rome_zk_layouts::{
    batch::{
        self as batch_layout, account_len_for, header_len, leaves_offset_for, write_header,
        write_header_v3, BatchDeposit, BatchFields, OFF_FINALIZED, VERSION as HEADER_V2,
        VERSION_V3 as HEADER_V3,
    },
    cursor,
    deposit::{self, queue_seed_hash, DepositRecord},
    deposit_queue::{deposit_queue as dq, deposit_record as dr},
    exit::exit_config as xc,
};
use rome_zk_testkit::{cursor_account_for, rent_exempt};
use solana_program::{
    clock::Clock,
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
};
use solana_sdk::{
    account::{Account, AccountSharedData},
    signature::{Keypair, Signer},
    transaction::TransactionError,
};
use solana_system_interface::program as system_program;
use zk_inbox::batch::BatchError;
use zk_inbox_client as client;

const CHAIN_ID: u64 = 9;
const BATCH: u64 = 3;
const OPEN_SLOT: u64 = 42;
const DEADLINE: u32 = 3_600;
const MAX_PER_BATCH: u16 = 4;
const MAX_PER_BLOCK: u16 = 2;

fn h(parts: &[&[u8]]) -> [u8; 32] {
    rome_zk_merkle::keccak256(parts)
}

fn put(ctx: &mut solana_program_test::ProgramTestContext, key: &Pubkey, account: Account) {
    ctx.set_account(key, &AccountSharedData::from(account));
}

fn plain(owner: Pubkey, data: Vec<u8>) -> Account {
    Account {
        lamports: rent_exempt(data.len()),
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

/// The error a `BatchError` becomes on chain.
fn named(e: BatchError) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(e as u32))
}

/// Deposit `i`, with fixed fields: nothing is derived from a secret.
fn record_of(i: u64) -> DepositRecord {
    DepositRecord {
        sender: [i as u8 + 1; 32],
        recipient: [i as u8 + 0x40; 20],
        amount_gwei: 1_000 + i,
    }
}

/// The hash chain over `n` deposits: `h_0` first, `h_n` last.
fn chain_values(
    settlement_program: &Pubkey,
    chain_id: u64,
    records: &[DepositRecord],
) -> Vec<[u8; 32]> {
    let sp = settlement_program.to_bytes();
    let hash = rome_zk_merkle::keccak256;
    let mut out = vec![queue_seed_hash(&hash, &sp, chain_id)];
    for k in 0..records.len() {
        let next = deposit::chain_through(&hash, &sp, chain_id, k as u64, &out[k], &records[k..=k]);
        out.push(next);
    }
    out
}

#[derive(Clone, Copy)]
enum CursorKind {
    /// A 21-byte v1 cursor holding `lamports`.
    V1 { lamports: u64 },
    /// A 69-byte v2 cursor at `deposit_next` with the chain value of that index.
    V2 { next: u64 },
}

#[derive(Clone, Copy)]
enum ExitCfg {
    /// No account at the PDA.
    Absent,
    /// An exit config owned by settlement that names the bridge.
    Bridge,
    /// An exit config owned by settlement whose bridge program is zero.
    ZeroBridge,
}

struct Cfg {
    /// The batch's header version.
    header: u8,
    leaves: u32,
    cursor: CursorKind,
    exit_config: ExitCfg,
    /// The queue's records, each with its age in seconds at the start; `None` has no queue account.
    queue: Option<Vec<(DepositRecord, i64)>>,
    max_per_batch: u16,
    max_per_block: u16,
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    /// When the batch was opened, in seconds after the clock's reading at the start.
    opened_after: i64,
    /// The batch's sealed leaves, when a test brings its own; otherwise `leaves` made-up ones.
    leaf_values: Option<Vec<[u8; 32]>>,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            header: HEADER_V3,
            leaves: 2,
            cursor: CursorKind::V2 { next: 0 },
            exit_config: ExitCfg::Bridge,
            queue: None,
            max_per_batch: MAX_PER_BATCH,
            max_per_block: MAX_PER_BLOCK,
            chain_id: CHAIN_ID,
            batch: BATCH,
            open_slot: OPEN_SLOT,
            opened_after: 0,
            leaf_values: None,
        }
    }
}

struct World {
    ctx: solana_program_test::ProgramTestContext,
    authority: Keypair,
    program_id: Pubkey,
    settlement_program: Pubkey,
    bridge: Pubkey,
    /// The chain values `h_0..=h_count` of the queue's records.
    chain: Vec<[u8; 32]>,
    leaves: Vec<[u8; 32]>,
    chain_id: u64,
    batch: u64,
    open_slot: u64,
    /// The batch header's committed open time.
    open_unix_ts: i64,
}

fn leaf_hashes(n: u32) -> Vec<[u8; 32]> {
    (0..n).map(|i| h(&[b"leaf", &i.to_le_bytes()])).collect()
}

fn pdas(w: &World) -> (Pubkey, Pubkey, Pubkey) {
    (
        client::batch_pda(&w.program_id, &w.settlement_program, w.chain_id, w.batch).0,
        client::cursor_pda(&w.program_id, &w.settlement_program, w.chain_id).0,
        client::exit_config_pda(&w.settlement_program, w.chain_id).0,
    )
}

fn queue_pda(w: &World) -> Pubkey {
    dq::pda(&w.bridge, &w.settlement_program.to_bytes(), w.chain_id).0
}

fn record_pda(w: &World, index: u64) -> Pubkey {
    dr::pda(
        &w.bridge,
        &w.settlement_program.to_bytes(),
        w.chain_id,
        index,
    )
    .0
}

fn queue_account(
    w: &World,
    count: u64,
    head: [u8; 32],
    max_per_batch: u16,
    max_per_block: u16,
) -> Account {
    let mut d = vec![0u8; dq::LEN];
    let params = dq::DepositParams {
        inclusion_deadline_secs: DEADLINE,
        max_per_batch,
        max_per_block,
        min_amount: 1,
        fee_lamports: 0,
        fee_recipient: [0; 32],
    };
    dq::write(
        &mut d,
        &dq::DepositQueueFields {
            count,
            head_hash: head,
            params,
            pending: dq::DepositParams::default(),
            activation_slot: 0,
        },
    );
    plain(w.bridge, d)
}

fn record_account(
    bridge: Pubkey,
    index: u64,
    ts: i64,
    r: &DepositRecord,
    hash_after: [u8; 32],
) -> Account {
    let mut d = vec![0u8; dr::LEN];
    dr::write(
        &mut d,
        &dr::DepositRecordFields {
            index,
            enqueue_unix_ts: ts,
            sender: r.sender,
            recipient: r.recipient,
            amount_gwei: r.amount_gwei,
            hash_after,
        },
    );
    plain(bridge, d)
}

fn exit_config_account(w: &World, bridge: [u8; 32]) -> Account {
    let d = xc::write(&xc::ExitConfigFields {
        chain_id: w.chain_id,
        exit_portal: [7; 20],
        bridge_program: bridge,
        pending_exit_portal: [0; 20],
        pending_bridge_program: [0; 32],
        pending_exit_cap: 0,
        pending_poster_bond: 0,
        activation_slot: 0,
        pending_mask: 0,
    });
    plain(w.settlement_program, d.to_vec())
}

/// A batch with every leaf sealed and nothing transformed yet, ready for `FinalizeBatchV2`.
fn batch_account(w: &World, header: u8, leaves: &[[u8; 32]]) -> Account {
    let n = leaves.len() as u32;
    let fields = BatchFields {
        chain_id: w.chain_id,
        batch: w.batch,
        open_slot: w.open_slot,
        expected_count: n,
        leaves_present: n,
        finalized: false,
        settlement_program: w.settlement_program.to_bytes(),
        authority: w.authority.pubkey().to_bytes(),
        root: [0; 32],
        forced_root: [0; 32],
        acc: [0; 32],
        finalize_cursor: 0,
        open_unix_ts: w.open_unix_ts,
        deposit: (header == HEADER_V3).then_some(BatchDeposit {
            from: 0,
            to: 0,
            hash_from: [0; 32],
            hash_to: [0; 32],
        }),
    };
    let head: Vec<u8> = if header == HEADER_V3 {
        write_header_v3(&fields).unwrap().to_vec()
    } else {
        write_header(&fields).to_vec()
    };
    let mut d = vec![0u8; account_len_for(header, n).unwrap()];
    d[..head.len()].copy_from_slice(&head);
    let bitmap = header_len(header).unwrap();
    let lo = leaves_offset_for(header, n).unwrap();
    for (i, leaf) in leaves.iter().enumerate() {
        d[bitmap + i / 8] |= 1 << (i % 8);
        d[lo + 32 * i..lo + 32 * (i + 1)].copy_from_slice(leaf);
    }
    plain(w.program_id, d)
}

async fn world(cfg: Cfg) -> World {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    let ctx = pt.start_with_context().await;
    let now = ctx
        .banks_client
        .get_sysvar::<Clock>()
        .await
        .unwrap()
        .unix_timestamp;
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let records: Vec<DepositRecord> = cfg.queue.iter().flatten().map(|(r, _)| *r).collect();
    let chain = chain_values(&settlement_program, cfg.chain_id, &records);
    let leaves = cfg
        .leaf_values
        .clone()
        .unwrap_or_else(|| leaf_hashes(cfg.leaves));
    let w = World {
        ctx,
        authority: Keypair::new(),
        program_id,
        settlement_program,
        bridge: rome_zk_testkit::fixed_zk_bridge_program_id(),
        chain,
        leaves,
        chain_id: cfg.chain_id,
        batch: cfg.batch,
        open_slot: cfg.open_slot,
        open_unix_ts: now + cfg.opened_after,
    };
    let mut w = w;
    let (batch, cursor_key, exit_config) = pdas(&w);

    let leaves = w.leaves.clone();
    let acct = batch_account(&w, cfg.header, &leaves);
    put(&mut w.ctx, &batch, acct);

    let seed = queue_seed_hash(
        &rome_zk_merkle::keccak256,
        &w.settlement_program.to_bytes(),
        w.chain_id,
    );
    let cursor_acct = match cfg.cursor {
        CursorKind::V1 { lamports } => {
            let mut a = cursor_account_for(1, w.program_id, w.chain_id, w.batch);
            a.lamports = lamports;
            a
        }
        CursorKind::V2 { next } => {
            let mut a = cursor_account_for(2, w.program_id, w.chain_id, w.batch);
            let chain_next = w.chain.get(next as usize).copied().unwrap_or(seed);
            a.data[cursor::OFF_DEPOSIT_NEXT..cursor::OFF_DEPOSIT_NEXT + 8]
                .copy_from_slice(&next.to_le_bytes());
            a.data[cursor::OFF_DEPOSIT_HASH..cursor::OFF_DEPOSIT_HASH + 32]
                .copy_from_slice(&chain_next);
            a
        }
    };
    put(&mut w.ctx, &cursor_key, cursor_acct);

    match cfg.exit_config {
        ExitCfg::Absent => {}
        ExitCfg::Bridge => {
            let a = exit_config_account(&w, w.bridge.to_bytes());
            put(&mut w.ctx, &exit_config, a);
        }
        ExitCfg::ZeroBridge => {
            let a = exit_config_account(&w, [0; 32]);
            put(&mut w.ctx, &exit_config, a);
        }
    }
    if let Some(queue) = &cfg.queue {
        let head = *w.chain.last().unwrap();
        let q = queue_account(
            &w,
            queue.len() as u64,
            head,
            cfg.max_per_batch,
            cfg.max_per_block,
        );
        let qk = queue_pda(&w);
        put(&mut w.ctx, &qk, q);
        for (i, (r, age)) in queue.iter().enumerate() {
            let a = record_account(w.bridge, i as u64, now - age, r, w.chain[i + 1]);
            let rk = record_pda(&w, i as u64);
            put(&mut w.ctx, &rk, a);
        }
    }
    w
}

/// `FinalizeBatchV2` for the world's batch, with the bridge named when the chain has a queue.
fn finalize_ix(w: &World, step: u32, deposit_to: u64, with_bridge: bool) -> Instruction {
    client::finalize_batch_v2_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        w.chain_id,
        w.batch,
        step,
        deposit_to,
        with_bridge.then_some(&w.bridge),
    )
}

async fn run(w: &mut World, ix: Instruction) -> Result<u64, TransactionError> {
    let payer = w.ctx.payer.insecure_clone();
    let authority = w.authority.insecure_clone();
    let (result, cu, _) =
        rome_zk_testkit::send_measuring_cu(&mut w.ctx, &[ix], &payer, &[&authority]).await;
    result.map(|()| cu)
}

async fn fin(
    w: &mut World,
    step: u32,
    to: u64,
    with_bridge: bool,
) -> Result<u64, TransactionError> {
    let ix = finalize_ix(w, step, to, with_bridge);
    run(w, ix).await
}

async fn account_at(w: &mut World, key: impl Fn(&World) -> Pubkey) -> Account {
    let k = key(w);
    account(w, k).await
}

async fn account(w: &mut World, key: Pubkey) -> Account {
    w.ctx.banks_client.get_account(key).await.unwrap().unwrap()
}

async fn batch_fields(w: &mut World) -> batch_layout::BatchFields {
    let (batch, _, _) = pdas(w);
    batch_layout::read(&account(w, batch).await.data).unwrap()
}

async fn cursor_fields(w: &mut World) -> (usize, cursor::CursorFields) {
    let (_, key, _) = pdas(w);
    let a = account(w, key).await;
    (a.data.len(), cursor::read(&a.data).unwrap())
}

/// The root of the world's leaves after the finalize transform, computed independently of the program.
fn expected_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let hash = rome_zk_merkle::keccak256;
    let t: Vec<[u8; 32]> = leaves
        .iter()
        .enumerate()
        .map(|(i, l)| rome_zk_merkle::indexed_leaf(&hash, i as u32, l))
        .collect();
    rome_zk_merkle::root(&hash, &t)
}

fn queue_with(n: u64, age: i64) -> Option<Vec<(DepositRecord, i64)>> {
    Some((0..n).map(|i| (record_of(i), age)).collect())
}

// ------------------------------------------------------------------------------------------------
// the empty range
// ------------------------------------------------------------------------------------------------

/// A chain with no queue and no exit config: the empty-range V2 finalize gives exactly the forced_root
/// and acc a `FinalizeBatch` always gave (the empty forced root and the accumulator formula over the
/// transformed leaves), on a v3 header and on a v2 header alike.
#[tokio::test]
async fn empty_range_finalize_gives_the_forced_root_and_acc_of_a_batch_without_deposits() {
    let hash = rome_zk_merkle::keccak256;
    let mut seen = Vec::new();
    for header in [HEADER_V3, HEADER_V2] {
        let mut w = world(Cfg {
            header,
            exit_config: ExitCfg::Absent,
            ..Cfg::default()
        })
        .await;
        let cu = fin(&mut w, 0, 0, false).await.unwrap();
        eprintln!(
            "FinalizeBatchV2 (header v{header}, 2 leaves, empty range, no queue) consumed {cu} CU"
        );
        let f = batch_fields(&mut w).await;
        let root = expected_root(&w.leaves);
        let forced = rome_zk_layouts::forced_empty_root(&hash);
        let acc = rome_zk_layouts::acc(&hash, CHAIN_ID, BATCH, OPEN_SLOT, 2, &root, &forced);
        assert!(f.finalized);
        assert_eq!((f.root, f.forced_root, f.acc), (root, forced, acc));
        let seed = queue_seed_hash(&hash, &w.settlement_program.to_bytes(), CHAIN_ID);
        match header {
            HEADER_V3 => assert_eq!(
                f.deposit,
                Some(BatchDeposit {
                    from: 0,
                    to: 0,
                    hash_from: seed,
                    hash_to: seed
                })
            ),
            _ => assert_eq!(f.deposit, None),
        }
        seen.push((f.root, f.forced_root, f.acc));
        // The cursor's deposit side does not move on an empty range.
        let (len, c) = cursor_fields(&mut w).await;
        assert_eq!(len, cursor::LEN_V2);
        assert_eq!(c.deposit.map(|d| (d.next, d.hash)), Some((0, seed)));
    }
    assert_eq!(
        seen[0], seen[1],
        "a v2 and a v3 header give the same root, forced_root and acc"
    );
}

/// The retired instruction is refused by name before any account is read.
#[tokio::test]
async fn finalize_batch_six_is_refused_by_name() {
    let mut w = world(Cfg::default()).await;
    let ix = client::finalize_batch_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        CHAIN_ID,
        BATCH,
        0,
    );
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        named(BatchError::RetiredInstruction)
    );
    assert!(!batch_fields(&mut w).await.finalized);
}

// ------------------------------------------------------------------------------------------------
// a non-empty range
// ------------------------------------------------------------------------------------------------

/// Three deposits, to == count: the range is written to the header, the forced_root is built by the shared
/// deposit functions, and the cursor moves.
#[tokio::test]
async fn a_range_reaching_the_queue_end_finalizes_and_moves_the_cursor() {
    let hash = rome_zk_merkle::keccak256;
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let cu = fin(&mut w, 0, 3, true).await.unwrap();
    eprintln!("FinalizeBatchV2 (2 leaves, 3 deposits) consumed {cu} CU");
    let f = batch_fields(&mut w).await;
    let (h0, h3) = (w.chain[0], w.chain[3]);
    let forced = deposit::forced_root(&hash, 0, 3, &h0, &h3);
    assert_ne!(forced, rome_zk_layouts::forced_empty_root(&hash));
    let root = expected_root(&w.leaves);
    assert_eq!(f.forced_root, forced);
    assert_eq!(
        f.acc,
        rome_zk_layouts::acc(&hash, CHAIN_ID, BATCH, OPEN_SLOT, 2, &root, &forced)
    );
    assert_eq!(
        f.deposit,
        Some(BatchDeposit {
            from: 0,
            to: 3,
            hash_from: h0,
            hash_to: h3
        })
    );
    let (_, c) = cursor_fields(&mut w).await;
    let d = c.deposit.unwrap();
    assert_eq!((d.next, d.hash, d.final_), (3, h3, 0));
    assert_eq!(
        c.next_batch, BATCH,
        "the batch cursor itself is not the deposit cursor"
    );
}

/// The range starts where the cursor stopped: a second batch takes deposits 2..4 of a four-deposit queue and
/// its hash_from is the chain value after deposit 1.
#[tokio::test]
async fn the_range_starts_at_the_cursors_deposit_next() {
    let hash = rome_zk_merkle::keccak256;
    let mut w = world(Cfg {
        cursor: CursorKind::V2 { next: 2 },
        queue: queue_with(4, 10),
        ..Cfg::default()
    })
    .await;
    fin(&mut w, 0, 4, true).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert_eq!(
        f.deposit,
        Some(BatchDeposit {
            from: 2,
            to: 4,
            hash_from: w.chain[2],
            hash_to: w.chain[4]
        })
    );
    assert_eq!(
        f.forced_root,
        deposit::forced_root(&hash, 2, 4, &w.chain[2], &w.chain[4])
    );
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| (d.next, d.hash)), Some((4, w.chain[4])));
}

/// A range that stops short of the queue's end is accepted by each of the three deadline clauses, one at a
/// time, and refused when none holds.
#[tokio::test]
async fn the_deadline_clauses() {
    // Five deposits, to = 2 stops short of the end. The default max_per_block is 2.
    let old = i64::from(DEADLINE) + 5;
    let young = i64::from(DEADLINE) - 100;
    let cases: [(&str, u64, i64, u16, Option<BatchError>); 6] = [
        // The next deposit (index 2) is inside its window: clause (c).
        ("next deposit inside its window", 2, young, 2, None),
        // Overdue, but the range took max_per_block deposits: clause (b).
        ("overdue, range at max_per_block", 2, old, 2, None),
        // Overdue and the range took fewer than max_per_block: refused.
        (
            "overdue, range under max_per_block",
            1,
            old,
            2,
            Some(BatchError::DepositDeadlineMissed),
        ),
        // An empty range with an overdue deposit waiting: refused.
        (
            "overdue, empty range",
            0,
            old,
            2,
            Some(BatchError::DepositDeadlineMissed),
        ),
        // The window is closed the second the age reaches the deadline.
        (
            "age equal to the deadline",
            1,
            i64::from(DEADLINE),
            2,
            Some(BatchError::DepositDeadlineMissed),
        ),
        // Overdue, but with max_per_block 1 a one-deposit range is a full block.
        ("overdue, range at max_per_block of one", 1, old, 1, None),
    ];
    for (name, to, age, per_block, want) in cases {
        let mut w = world(Cfg {
            queue: queue_with(5, age),
            max_per_block: per_block,
            ..Cfg::default()
        })
        .await;
        let got = fin(&mut w, 0, to, true).await;
        match want {
            None => {
                got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            }
            Some(e) => assert_eq!(got.unwrap_err(), named(e), "{name}"),
        }
    }
    // Clause (a): the range reaches the queue's end, however old the deposits are.
    let mut w = world(Cfg {
        queue: queue_with(1, old),
        ..Cfg::default()
    })
    .await;
    fin(&mut w, 0, 1, true).await.unwrap();
}

#[tokio::test]
async fn a_range_refusal_names_its_rule() {
    // from = the cursor's deposit_next: a `to` below it is refused.
    let mut w = world(Cfg {
        cursor: CursorKind::V2 { next: 2 },
        queue: queue_with(4, 10),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::DepositToBelowFrom)
    );
    // to above the queue's count.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 4, true).await.unwrap_err(),
        named(BatchError::DepositToAboveCount)
    );
    // to - from above max_per_batch (here 2).
    let mut w = world(Cfg {
        queue: queue_with(4, 10),
        max_per_batch: 2,
        max_per_block: 2,
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 3, true).await.unwrap_err(),
        named(BatchError::DepositRangeTooLarge)
    );
    // Exactly max_per_batch is fine.
    fin(&mut w, 0, 2, true).await.unwrap();
    // None of the refusals finalized anything: the batch above is the only one that did.
}

/// A refused finalize moves neither the batch nor the cursor.
#[tokio::test]
async fn a_refused_finalize_changes_nothing() {
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let (batch, cursor_key, _) = pdas(&w);
    let before = (
        account(&mut w, batch).await.data,
        account(&mut w, cursor_key).await.data,
    );
    fin(&mut w, 0, 4, true).await.unwrap_err();
    let after = (
        account(&mut w, batch).await.data,
        account(&mut w, cursor_key).await.data,
    );
    assert_eq!(before, after);
}

// ------------------------------------------------------------------------------------------------
// the records
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_record_at_the_wrong_address_or_owner_is_refused() {
    // record(to - 1) at another address.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let mut ix = finalize_ix(&w, 0, 2, true);
    let stray = Pubkey::new_unique();
    let a = account_at(&mut w, |w| record_pda(w, 1)).await;
    put(&mut w.ctx, &stray, a);
    ix.accounts[5] = AccountMeta::new_readonly(stray, false);
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        named(BatchError::WrongDepositRecordAddress)
    );

    // record(to) at another address: the deadline check reads it when the range stops short.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let mut ix = finalize_ix(&w, 0, 1, true);
    let stray = Pubkey::new_unique();
    let a = account_at(&mut w, |w| record_pda(w, 1)).await;
    put(&mut w.ctx, &stray, a);
    ix.accounts[6] = AccountMeta::new_readonly(stray, false);
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        named(BatchError::WrongDepositRecordAddress)
    );

    // record(to - 1) at its address, owned by another program.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let key = record_pda(&w, 1);
    let mut a = account(&mut w, key).await;
    a.owner = Pubkey::new_unique();
    put(&mut w.ctx, &key, a);
    assert_eq!(
        fin(&mut w, 0, 2, true).await.unwrap_err(),
        named(BatchError::WrongDepositRecordOwner)
    );

    // record(to) owned by another program.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let key = record_pda(&w, 1);
    let mut a = account(&mut w, key).await;
    a.owner = Pubkey::new_unique();
    put(&mut w.ctx, &key, a);
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::WrongDepositRecordOwner)
    );
}

// ------------------------------------------------------------------------------------------------
// no queue, a v2 header
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_non_empty_range_needs_a_queue() {
    // No exit config at all.
    let mut w = world(Cfg {
        exit_config: ExitCfg::Absent,
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 1, false).await.unwrap_err(),
        named(BatchError::NoDepositQueue)
    );
    // An exit config whose bridge is zero.
    let mut w = world(Cfg {
        exit_config: ExitCfg::ZeroBridge,
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 1, false).await.unwrap_err(),
        named(BatchError::NoDepositQueue)
    );
    // An exit config that names the bridge, and no queue account.
    let mut w = world(Cfg::default()).await;
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::NoDepositQueue)
    );
    // The empty range is fine in all three.
    fin(&mut w, 0, 0, true).await.unwrap();
}

#[tokio::test]
async fn a_v2_header_takes_only_an_empty_range() {
    let mut w = world(Cfg {
        header: HEADER_V2,
        queue: queue_with(2, 10),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 2, true).await.unwrap_err(),
        named(BatchError::HeaderV2TakesEmptyRange)
    );
    // Even with an old deposit waiting, an empty range finalizes the v2 batch: its blocks were sealed before
    // any queue existed, so the deadline does not apply to it.
    let mut w = world(Cfg {
        header: HEADER_V2,
        queue: queue_with(1, i64::from(DEADLINE) + 5),
        ..Cfg::default()
    })
    .await;
    fin(&mut w, 0, 0, true).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert!(f.finalized);
    assert_eq!(f.deposit, None);
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| d.next), Some(0));
    // The same batch still refuses a non-empty range, overdue deposit or not.
    let mut w = world(Cfg {
        header: HEADER_V2,
        queue: queue_with(1, i64::from(DEADLINE) + 5),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::HeaderV2TakesEmptyRange)
    );
    // With nothing overdue it finalizes and the cursor stays put.
    let mut w = world(Cfg {
        header: HEADER_V2,
        queue: queue_with(1, 10),
        ..Cfg::default()
    })
    .await;
    fin(&mut w, 0, 0, true).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert!(f.finalized);
    assert_eq!(f.deposit, None);
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| d.next), Some(0));
}

// ------------------------------------------------------------------------------------------------
// the deadline is measured at the batch's open time
// ------------------------------------------------------------------------------------------------

/// A batch that can finalize when it is open still can after the batcher has been away for a long time: the
/// clock moving forward changes nothing, because the age is taken at the batch's own open time.
#[tokio::test]
async fn an_open_batch_still_finalizes_after_the_clock_moves_forward() {
    // Two deposits are queued and the batch's stream ends after the first, so `to = 1` stops short of the
    // queue with fewer than `max_per_block` deposits: it needs the second deposit to be inside its window.
    let mut w = world(Cfg {
        queue: queue_with(2, 10),
        ..Cfg::default()
    })
    .await;
    let mut clock = w.ctx.banks_client.get_sysvar::<Clock>().await.unwrap();
    // The finalize runs far more than the deadline after the batch opened.
    clock.unix_timestamp += i64::from(DEADLINE) * 3;
    w.ctx.set_sysvar(&clock);
    fin(&mut w, 0, 1, true).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert!(f.finalized);
    assert_eq!(f.deposit.map(|d| (d.from, d.to)), Some((0, 1)));
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| d.next), Some(1));
}

/// The verdict is the batch's own from the moment it opens: a batch opened after a deposit's deadline cannot
/// take an empty range or a range that leaves that deposit out, and one opened a second earlier can.
#[tokio::test]
async fn a_batch_opened_after_a_deposits_deadline_cannot_take_an_empty_range() {
    // One deposit enqueued 10 s before the clock's start; its deadline falls DEADLINE - 10 s after it.
    let cases: [(&str, i64, bool); 4] = [
        ("opened with the deposit still young", 0, true),
        (
            "opened one second before the deadline",
            i64::from(DEADLINE) - 11,
            true,
        ),
        ("opened on the deadline", i64::from(DEADLINE) - 10, false),
        ("opened long after it", i64::from(DEADLINE) * 5, false),
    ];
    for (name, opened_after, finalizes) in cases {
        // Two deposits so that the empty range stops short of the queue's end.
        let mut w = world(Cfg {
            queue: queue_with(2, 10),
            opened_after,
            ..Cfg::default()
        })
        .await;
        let got = fin(&mut w, 0, 0, true).await;
        if finalizes {
            got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        } else {
            assert_eq!(
                got.unwrap_err(),
                named(BatchError::DepositDeadlineMissed),
                "{name}"
            );
        }
    }
    // Taking a deposit does not help when the next one is overdue at the open time and the block is not full.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        opened_after: i64::from(DEADLINE),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::DepositDeadlineMissed)
    );
}

// ------------------------------------------------------------------------------------------------
// the open-time verdict holds for a fixed grace
// ------------------------------------------------------------------------------------------------

const GRACE: i64 = zk_inbox::batch::DEPOSIT_GRACE_SECS;

/// Moves the cluster clock to `unix_timestamp`.
async fn set_clock_to(w: &mut World, unix_timestamp: i64) {
    let mut clock = w.ctx.banks_client.get_sysvar::<Clock>().await.unwrap();
    clock.unix_timestamp = unix_timestamp;
    w.ctx.set_sysvar(&clock);
}

/// A deposit enqueued one second after several batches opened, with a one hour deadline: the batches stay open
/// and each finalizes an empty range. That works for 24 hours past the deposit's deadline and not a second
/// longer, however many batches were opened early.
#[tokio::test]
async fn batches_opened_early_cannot_keep_a_deposit_out_past_the_grace() {
    let deadline = i64::from(DEADLINE);
    // The last second at which the deposit can still be left out.
    let cases: [(&str, i64, bool); 3] = [
        ("a second before the grace ends", GRACE + deadline - 1, true),
        ("when the grace ends", GRACE + deadline, false),
        ("thirty days on", 30 * 86_400, false),
    ];
    for (name, after_enqueue, accepted) in cases {
        for id in [4u64, 5, 6] {
            // Batches 4, 5 and 6 are open and opened at the start of the clock; deposit 0 is enqueued one
            // second later (a negative age is one second in the future).
            let mut w = world(Cfg {
                batch: 6,
                queue: Some(vec![(record_of(0), -1)]),
                ..Cfg::default()
            })
            .await;
            put_batch(&mut w, 3, true);
            put_batch(&mut w, 4, false);
            put_batch(&mut w, 5, false);
            // Batches before `id` are final, so only the grace is under test.
            for earlier in 4..id {
                put_batch(&mut w, earlier, true);
            }
            let start = w.open_unix_ts;
            set_clock_to(&mut w, start + 1 + after_enqueue).await;
            let got = fin_batch(&mut w, id, 0).await;
            if accepted {
                got.unwrap_or_else(|e| panic!("{name}, batch {id}: {e:?}"));
            } else {
                assert_eq!(
                    got.unwrap_err(),
                    named(BatchError::DepositDeadlineMissed),
                    "{name}, batch {id}"
                );
            }
        }
    }
}

/// A batch opened before a deposit's deadline can finalize empty for the whole grace after it, wherever the
/// clock is inside it, and not after.
#[tokio::test]
async fn a_batch_opened_before_the_deadline_finalizes_empty_anywhere_inside_the_grace() {
    let deadline = i64::from(DEADLINE);
    // Deposit 0 was enqueued 10 s before the batch opened, so its deadline is `deadline - 10` s after.
    let enqueued = -10i64;
    let cases: [(&str, i64, bool); 6] = [
        ("right after opening", 0, true),
        ("at the deposit's deadline", enqueued + deadline, true),
        ("an hour past the deadline", enqueued + 2 * deadline, true),
        ("inside the grace", GRACE, true),
        (
            "a second before the grace ends",
            enqueued + deadline + GRACE - 1,
            true,
        ),
        ("when the grace ends", enqueued + deadline + GRACE, false),
    ];
    for (name, clock_after_open, accepted) in cases {
        let mut w = world(Cfg {
            queue: queue_with(2, -enqueued),
            ..Cfg::default()
        })
        .await;
        let start = w.open_unix_ts;
        set_clock_to(&mut w, start + clock_after_open).await;
        let got = fin(&mut w, 0, 0, true).await;
        if accepted {
            got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        } else {
            assert_eq!(
                got.unwrap_err(),
                named(BatchError::DepositDeadlineMissed),
                "{name}"
            );
        }
    }
}

/// Taking the waiting deposit is accepted at any time: the grace only limits leaving it out.
#[tokio::test]
async fn taking_the_waiting_deposit_is_accepted_at_any_time() {
    for after_open in [0, i64::from(DEADLINE) * 2, GRACE * 2, 30 * 86_400] {
        // The range reaches the queue's end.
        let mut w = world(Cfg {
            queue: queue_with(1, 10),
            ..Cfg::default()
        })
        .await;
        let start = w.open_unix_ts;
        set_clock_to(&mut w, start + after_open).await;
        fin(&mut w, 0, 1, true)
            .await
            .unwrap_or_else(|e| panic!("to the end, {after_open} s after open: {e:?}"));
        // The range stops short of the end but fills the block.
        let mut w = world(Cfg {
            queue: queue_with(3, 10),
            max_per_block: 1,
            ..Cfg::default()
        })
        .await;
        let start = w.open_unix_ts;
        set_clock_to(&mut w, start + after_open).await;
        fin(&mut w, 0, 1, true)
            .await
            .unwrap_or_else(|e| panic!("a full block, {after_open} s after open: {e:?}"));
    }
}

// ------------------------------------------------------------------------------------------------
// the address bindings, on an empty-range finalize past a deadline
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_address_bindings_hold_on_an_empty_range_past_a_deadline() {
    let old = i64::from(DEADLINE) + 5;
    // Baseline: with the right accounts the overdue deposit blocks the empty range.
    let mut w = world(Cfg {
        queue: queue_with(2, old),
        ..Cfg::default()
    })
    .await;
    assert_eq!(
        fin(&mut w, 0, 0, true).await.unwrap_err(),
        named(BatchError::DepositDeadlineMissed)
    );

    // An exit_config at another address: refused, whether that account is empty or a copy of the real one.
    for copy in [false, true] {
        let mut w = world(Cfg {
            queue: queue_with(2, old),
            ..Cfg::default()
        })
        .await;
        let (_, _, exit_config) = pdas(&w);
        let stray = Pubkey::new_unique();
        if copy {
            let a = account(&mut w, exit_config).await;
            put(&mut w.ctx, &stray, a);
        }
        let mut ix = finalize_ix(&w, 0, 0, true);
        ix.accounts[3] = AccountMeta::new_readonly(stray, false);
        assert_eq!(
            run(&mut w, ix).await.unwrap_err(),
            named(BatchError::WrongExitConfigAddress),
            "copy {copy}"
        );
    }

    // A queue at another address: bridge-owned (a copy), system-owned (empty), and a missing account.
    for kind in ["bridge-owned", "system-owned", "missing"] {
        let mut w = world(Cfg {
            queue: queue_with(2, old),
            ..Cfg::default()
        })
        .await;
        let stray = Pubkey::new_unique();
        match kind {
            "bridge-owned" => {
                let a = account_at(&mut w, queue_pda).await;
                put(&mut w.ctx, &stray, a);
            }
            "system-owned" => put(&mut w.ctx, &stray, plain(system_program::id(), vec![])),
            _ => {}
        }
        let mut ix = finalize_ix(&w, 0, 0, true);
        ix.accounts[4] = AccountMeta::new_readonly(stray, false);
        assert_eq!(
            run(&mut w, ix).await.unwrap_err(),
            named(BatchError::WrongDepositQueueAddress),
            "{kind}"
        );
    }

    // The queue's own address, but owned by the system program (it counts as absent): an empty range
    // finalizes, and a non-empty one is refused as having no queue.
    let mut w = world(Cfg {
        queue: queue_with(2, old),
        ..Cfg::default()
    })
    .await;
    let qk = queue_pda(&w);
    put(&mut w.ctx, &qk, plain(system_program::id(), vec![]));
    assert_eq!(
        fin(&mut w, 0, 1, true).await.unwrap_err(),
        named(BatchError::NoDepositQueue)
    );
}

// ------------------------------------------------------------------------------------------------
// the cursor
// ------------------------------------------------------------------------------------------------

/// A v1 cursor holding the rent for 69 bytes grows to a v2 cursor on its first completing finalize.
#[tokio::test]
async fn a_v1_cursor_migrates_on_its_first_completing_finalize() {
    let hash = rome_zk_merkle::keccak256;
    for (to, queue) in [(0u64, None), (2, queue_with(2, 10))] {
        let mut w = world(Cfg {
            cursor: CursorKind::V1 {
                lamports: rent_exempt(cursor::LEN_V2),
            },
            queue,
            ..Cfg::default()
        })
        .await;
        let (_, cursor_key, _) = pdas(&w);
        let lamports = account(&mut w, cursor_key).await.lamports;
        // The exit config names a bridge, so the queue is bound by address even when it is absent.
        let cu = fin(&mut w, 0, to, true).await.unwrap();
        eprintln!("FinalizeBatchV2 migrating a v1 cursor (range 0..{to}) consumed {cu} CU");
        let a = account(&mut w, cursor_key).await;
        assert_eq!(a.data.len(), cursor::LEN_V2);
        assert_eq!(a.lamports, lamports, "the migration moves no lamports");
        let c = cursor::read(&a.data).unwrap();
        assert_eq!((c.chain_id, c.next_batch), (CHAIN_ID, BATCH));
        let seed = queue_seed_hash(&hash, &w.settlement_program.to_bytes(), CHAIN_ID);
        let want_hash = if to == 0 { seed } else { w.chain[2] };
        assert_eq!(
            c.deposit.map(|d| (d.next, d.hash, d.final_)),
            Some((to, want_hash, 0))
        );
        let f = batch_fields(&mut w).await;
        assert_eq!(
            f.deposit.map(|d| (d.from, d.to, d.hash_from)),
            Some((0, to, seed))
        );
    }
}

/// A v1 cursor short of the 69-byte rent minimum is refused by name, and nothing moves.
#[tokio::test]
async fn a_v1_cursor_short_of_rent_is_refused_by_name() {
    let mut w = world(Cfg {
        cursor: CursorKind::V1 {
            lamports: rent_exempt(cursor::LEN),
        },
        ..Cfg::default()
    })
    .await;
    let (batch, cursor_key, _) = pdas(&w);
    let before = (
        account(&mut w, batch).await.data,
        account(&mut w, cursor_key).await.data,
    );
    assert_eq!(
        fin(&mut w, 0, 0, true).await.unwrap_err(),
        named(BatchError::CursorShortOfRent)
    );
    let after = (
        account(&mut w, batch).await.data,
        account(&mut w, cursor_key).await.data,
    );
    assert_eq!(before, after);
    assert_eq!(after.1.len(), cursor::LEN);
    // A plain transfer of the shortfall is all the batcher needs to do.
    let shortfall = rent_exempt(cursor::LEN_V2) - rent_exempt(cursor::LEN);
    let payer = w.ctx.payer.insecure_clone();
    let topup =
        solana_system_interface::instruction::transfer(&payer.pubkey(), &cursor_key, shortfall);
    let (r, _, _) = rome_zk_testkit::send_measuring_cu(&mut w.ctx, &[topup], &payer, &[]).await;
    r.unwrap();
    fin(&mut w, 0, 0, true).await.unwrap();
    assert_eq!(account(&mut w, cursor_key).await.data.len(), cursor::LEN_V2);
}

/// The cursor is read only on the call that completes the batch.
#[tokio::test]
async fn an_earlier_step_reads_no_deposit_account() {
    let mut w = world(Cfg {
        queue: queue_with(1, 10),
        ..Cfg::default()
    })
    .await;
    let mut ix = finalize_ix(&w, 1, 0, true);
    // Replace the six accounts after the authority with the system program: an earlier step never looks.
    for meta in ix.accounts.iter_mut().skip(2) {
        *meta = AccountMeta::new_readonly(system_program::id(), false);
    }
    run(&mut w, ix).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert!(!f.finalized);
    assert_eq!(f.finalize_cursor, 1);
    // The completing call does read them.
    let mut ix = finalize_ix(&w, 1, 0, true);
    ix.accounts[2] = AccountMeta::new_readonly(system_program::id(), false);
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        TransactionError::InstructionError(0, InstructionError::IncorrectProgramId)
    );
    fin(&mut w, 1, 0, true).await.unwrap();
    assert!(batch_fields(&mut w).await.finalized);
}

// ------------------------------------------------------------------------------------------------
// batch order
// ------------------------------------------------------------------------------------------------

/// Puts a sealed, untransformed batch at `id` in the world, finalized or not, then leaves the world on its
/// own batch again.
fn put_batch(w: &mut World, id: u64, finalized: bool) {
    let own = w.batch;
    w.batch = id;
    let key = pdas(w).0;
    let leaves = w.leaves.clone();
    let mut acct = batch_account(w, HEADER_V3, &leaves);
    acct.data[OFF_FINALIZED] = u8::from(finalized);
    put(&mut w.ctx, &key, acct);
    w.batch = own;
}

/// Runs `finalize` for batch `id` and returns to the world's own batch.
async fn fin_batch(w: &mut World, id: u64, to: u64) -> Result<u64, TransactionError> {
    let own = w.batch;
    w.batch = id;
    let r = fin(w, 0, to, true).await;
    w.batch = own;
    r
}

/// Batch 4 cannot take deposits 0..2 while batch 3 is open: the later batch would settle first and L2 would
/// credit deposit 2 before 0 and 1. Finalizing in id order works, and the second batch carries on from the
/// first one's range.
#[tokio::test]
async fn a_batch_cannot_take_deposits_before_the_batch_ahead_of_it_is_final() {
    let mut w = world(Cfg {
        batch: 4,
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, 3, false);
    let (_, cursor_key, _) = pdas(&w);
    let before = account(&mut w, cursor_key).await.data;
    assert_eq!(
        fin(&mut w, 0, 2, true).await.unwrap_err(),
        named(BatchError::PreviousBatchNotFinalized)
    );
    assert!(!batch_fields(&mut w).await.finalized);
    assert_eq!(account(&mut w, cursor_key).await.data, before);

    // In id order: batch 3 takes 0..2, then batch 4 takes 2..3.
    fin_batch(&mut w, 3, 2).await.unwrap();
    fin(&mut w, 0, 3, true).await.unwrap();
    let f = batch_fields(&mut w).await;
    assert_eq!(f.deposit.map(|d| (d.from, d.to)), Some((2, 3)));
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| d.next), Some(3));
}

/// The rule holds on an empty range, and an earlier step of a resumable finalize does not read the account.
#[tokio::test]
async fn the_batch_order_rule_holds_on_an_empty_range_and_only_on_the_completing_call() {
    let mut w = world(Cfg {
        batch: 4,
        exit_config: ExitCfg::Absent,
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, 3, false);
    // Step 1 of 2 leaves is not the completing call.
    fin(&mut w, 1, 0, false).await.unwrap();
    assert!(!batch_fields(&mut w).await.finalized);
    assert_eq!(
        fin(&mut w, 1, 0, false).await.unwrap_err(),
        named(BatchError::PreviousBatchNotFinalized)
    );
    // Once batch 3 is final the same call completes.
    put_batch(&mut w, 3, true);
    fin(&mut w, 1, 0, false).await.unwrap();
    assert!(batch_fields(&mut w).await.finalized);
}

/// The previous-batch slot must hold the address `batch - 1` derives to, whatever else sits there.
#[tokio::test]
async fn a_wrong_previous_batch_address_is_refused_by_name() {
    let mut w = world(Cfg {
        batch: 4,
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, 3, true);
    // A finalized batch from further back, a made-up key, and the batch itself.
    put_batch(&mut w, 2, true);
    let older = client::batch_pda(&w.program_id, &w.settlement_program, w.chain_id, 2).0;
    let own = pdas(&w).0;
    for wrong in [older, Pubkey::new_unique(), own, system_program::id()] {
        let mut ix = finalize_ix(&w, 0, 2, true);
        assert_eq!(ix.accounts.len(), 8);
        ix.accounts[7] = AccountMeta::new_readonly(wrong, false);
        assert_eq!(
            run(&mut w, ix).await.unwrap_err(),
            named(BatchError::WrongPreviousBatchAddress),
            "{wrong}"
        );
    }
    // The empty range is bound too.
    let mut ix = finalize_ix(&w, 0, 0, true);
    ix.accounts[7] = AccountMeta::new_readonly(older, false);
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        named(BatchError::WrongPreviousBatchAddress)
    );
    // The right address works.
    fin(&mut w, 0, 2, true).await.unwrap();
}

/// A previous batch that is gone (abandoned, or closed after it went final) or final does not hold the next
/// one back.
#[tokio::test]
async fn an_absent_or_final_previous_batch_is_accepted() {
    // Absent: nothing at batch 2's address.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let prev = client::batch_pda(&w.program_id, &w.settlement_program, w.chain_id, BATCH - 1).0;
    assert!(w
        .ctx
        .banks_client
        .get_account(prev)
        .await
        .unwrap()
        .is_none());
    fin(&mut w, 0, 2, true).await.unwrap();
    assert_eq!(
        batch_fields(&mut w).await.deposit.map(|d| (d.from, d.to)),
        Some((0, 2))
    );

    // Final: the previous batch is still there.
    let mut w = world(Cfg {
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, BATCH - 1, true);
    fin(&mut w, 0, 2, true).await.unwrap();
}

/// Batch 0 has no previous batch: the call completes whatever the slot holds, and the builder fills it with
/// the system program.
#[tokio::test]
async fn batch_zero_skips_the_previous_batch_check() {
    let mut w = world(Cfg {
        batch: 0,
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    let ix = finalize_ix(&w, 0, 2, true);
    assert_eq!(ix.accounts[7].pubkey, system_program::id());
    run(&mut w, ix).await.unwrap();
    assert_eq!(
        batch_fields(&mut w).await.deposit.map(|d| (d.from, d.to)),
        Some((0, 2))
    );
    // Not even an eighth account is needed.
    let mut w = world(Cfg {
        batch: 0,
        exit_config: ExitCfg::Absent,
        ..Cfg::default()
    })
    .await;
    let mut ix = finalize_ix(&w, 0, 0, false);
    ix.accounts.truncate(7);
    run(&mut w, ix).await.unwrap();
}

// ------------------------------------------------------------------------------------------------
// the synthetic small batch, byte for byte
// ------------------------------------------------------------------------------------------------

fn unhex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The three deposits of `fixtures/prover-input/synthetic-deposits-small`: senders are the fixed-seed depositors,
/// recipients and amounts are functions of the index.
fn synthetic_record(index: u64) -> DepositRecord {
    let r = h(&[b"synthetic-deposits/v1/recipient", &index.to_le_bytes()]);
    let mut recipient = [0u8; 20];
    recipient.copy_from_slice(&r[12..]);
    DepositRecord {
        sender: rome_zk_testkit::synthetic_depositor_pubkey(index),
        recipient,
        amount_gwei: 1_000_000 + 12_345 * index,
    }
}

/// The queue, the exit config and the three records are fixtures; the batch is chain 200101's batch 1, opened at
/// slot 1,000 with the fixture's one frame as its one leaf. Finalizing the range 0..3 has to give the fixture's
/// header range, forced_root, inbox root and acc exactly.
#[tokio::test]
async fn the_synthetic_small_batch_is_reproduced_byte_for_byte() {
    // keccak256 of the fixture's one 70-byte channel frame.
    let frame_hash = unhex32("d338633229e3b30cc4a5cf66afb715da426fd90946fd4c36693b47fcaccd3a77");
    let mut w = world(Cfg {
        chain_id: 200_101,
        batch: 1,
        open_slot: 1_000,
        leaves: 1,
        leaf_values: Some(vec![frame_hash]),
        cursor: CursorKind::V2 { next: 0 },
        queue: Some((0..3).map(|i| (synthetic_record(i), 10)).collect()),
        ..Cfg::default()
    })
    .await;
    assert_eq!(w.settlement_program.to_bytes(), [0x51u8; 32]);
    let h_0 = unhex32("035872f612eaf18c5769c8475143f8233f22d71e45b39a4e925cdf34896fa980");
    let h_to = unhex32("de2ada9842a4ee1472aaff3f3e50339efaf3c2d922a3aae2d84b4df87eb25fd9");
    // The fixtures' own chain, from the shared deposit functions, is the fixture's.
    assert_eq!(w.chain[0], h_0);
    assert_eq!(w.chain[3], h_to);

    fin(&mut w, 0, 3, true).await.unwrap();

    let f = batch_fields(&mut w).await;
    assert!(f.finalized);
    assert_eq!(
        f.root,
        unhex32("984c6880919725e19f23c9b427ca9c21535331b141fe4751e339a4b70d8c7d25")
    );
    assert_eq!(
        f.forced_root,
        unhex32("8bb7e79bf9767f0cc2772f2e2ee2c8d389cbdf8e87138488c738c50aeff3be04")
    );
    assert_eq!(
        f.acc,
        unhex32("e92a3f30c5aa39e79c7b849892f81036a0e15090e81dcd2cab4357dd89843843")
    );
    assert_eq!(
        f.deposit,
        Some(BatchDeposit {
            from: 0,
            to: 3,
            hash_from: h_0,
            hash_to: h_to
        })
    );
    let (_, c) = cursor_fields(&mut w).await;
    assert_eq!(c.deposit.map(|d| (d.next, d.hash)), Some((3, h_to)));
}

// ------------------------------------------------------------------------------------------------
// abandoning a batch keeps the order rule
// ------------------------------------------------------------------------------------------------

/// Builds `AbandonBatch` for batch `id` as the world's authority.
fn abandon_ix(w: &World, id: u64) -> Instruction {
    client::abandon_batch_ix(
        &w.program_id,
        &w.authority.pubkey(),
        &w.settlement_program,
        w.chain_id,
        id,
    )
}

/// Batches 3 and 4 are open: abandoning 4 is refused while 3 is open, so batch 5 can never see an absent
/// predecessor while an earlier batch is still open. Nothing changes on the refusal, and once batch 3 is final
/// the same call works.
#[tokio::test]
async fn a_batch_cannot_be_abandoned_while_the_batch_ahead_of_it_is_open() {
    let mut w = world(Cfg {
        batch: 5,
        queue: queue_with(3, 10),
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, 3, false);
    put_batch(&mut w, 4, false);
    let four = client::batch_pda(&w.program_id, &w.settlement_program, w.chain_id, 4).0;
    let before = account(&mut w, four).await;
    let ix = abandon_ix(&w, 4);
    assert_eq!(
        run(&mut w, ix).await.unwrap_err(),
        named(BatchError::PreviousBatchNotFinalized)
    );
    let after = account(&mut w, four).await;
    assert_eq!(after.data, before.data);
    assert_eq!(after.lamports, before.lamports);
    assert_eq!(after.owner, before.owner);

    // Batch 5 still sees batch 4 open, so it cannot take the range ahead of batch 3.
    assert_eq!(
        fin(&mut w, 0, 2, true).await.unwrap_err(),
        named(BatchError::PreviousBatchNotFinalized)
    );

    // Batch 3 final: batch 4 may go.
    put_batch(&mut w, 3, true);
    let ix = abandon_ix(&w, 4);
    run(&mut w, ix).await.unwrap();
    assert!(w
        .ctx
        .banks_client
        .get_account(four)
        .await
        .unwrap()
        .is_none());
}

/// A previous batch that is final, or absent, or a batch 0 with no predecessor, does not stop an abandon; the
/// previous-batch slot must hold the address `batch - 1` derives to.
#[tokio::test]
async fn abandoning_a_batch_works_when_the_previous_batch_is_final_or_absent_and_binds_the_address()
{
    // Previous batch final.
    let mut w = world(Cfg::default()).await;
    put_batch(&mut w, BATCH - 1, true);
    put_batch(&mut w, BATCH, false);
    let ix = abandon_ix(&w, BATCH);
    assert_eq!(ix.accounts.len(), 3);
    run(&mut w, ix).await.unwrap();

    // Previous batch absent.
    let mut w = world(Cfg::default()).await;
    put_batch(&mut w, BATCH, false);
    let ix = abandon_ix(&w, BATCH);
    run(&mut w, ix).await.unwrap();

    // Batch 0: the slot carries the system program and is not read.
    let mut w = world(Cfg {
        batch: 0,
        ..Cfg::default()
    })
    .await;
    put_batch(&mut w, 0, false);
    let ix = abandon_ix(&w, 0);
    assert_eq!(ix.accounts[2].pubkey, system_program::id());
    run(&mut w, ix).await.unwrap();

    // Another address in the slot is refused by name, even one that holds a final batch.
    let mut w = world(Cfg::default()).await;
    put_batch(&mut w, BATCH - 2, true);
    put_batch(&mut w, BATCH, false);
    let older = client::batch_pda(&w.program_id, &w.settlement_program, w.chain_id, BATCH - 2).0;
    for wrong in [older, Pubkey::new_unique(), system_program::id()] {
        let mut ix = abandon_ix(&w, BATCH);
        ix.accounts[2] = AccountMeta::new_readonly(wrong, false);
        assert_eq!(
            run(&mut w, ix).await.unwrap_err(),
            named(BatchError::WrongPreviousBatchAddress),
            "{wrong}"
        );
    }
}

// ------------------------------------------------------------------------------------------------
// CU
// ------------------------------------------------------------------------------------------------

/// The worst case: 900 leaves, a v1 cursor that migrates, and a three-deposit range that reads three records.
#[tokio::test]
async fn a_900_leaf_finalize_with_deposits_and_a_migration_stays_under_the_gate() {
    let mut w = world(Cfg {
        leaves: 900,
        cursor: CursorKind::V1 {
            lamports: rent_exempt(cursor::LEN_V2),
        },
        queue: queue_with(5, 10),
        // `max_per_block` above the three deposits taken, so the deadline reads `record(to)` as well.
        max_per_block: 4,
        ..Cfg::default()
    })
    .await;
    // to = 3 stops short of the end and below `max_per_block`, so record(to) is read for the deadline and
    // record(to - 1) for the hash.
    let cu = fin(&mut w, 0, 3, true).await.unwrap();
    eprintln!("FinalizeBatchV2 (900 leaves, v1 cursor migrating, range 0..3 of 5) consumed {cu} CU (gate: 600,000)");
    assert!(cu < 600_000, "{cu} CU is over the 600,000 gate");
    let f = batch_fields(&mut w).await;
    assert_eq!(f.root, expected_root(&w.leaves));
    assert_eq!(f.deposit.map(|d| (d.from, d.to)), Some((0, 3)));
}
