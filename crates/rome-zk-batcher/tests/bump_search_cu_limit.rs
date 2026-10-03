//! The batcher's chunk-lane compute-unit limit (`Config::chunk_compute_unit_limit`) covers two transactions: the
//! open-and-grow transaction (`OpenBatch` plus the `GrowBatch` instructions a 900-leaf batch needs) and every
//! frame's chunk transaction (`Open`, `Write`, `Seal`, `SealLeaf`).
//!
//! Both derive program addresses inside the program, and a derivation costs 1,500 CU for every extra bump
//! attempt. The attempts are fixed per address, so one batch id (or one chunk slot) can cost far more than the
//! next. The batch cursor cannot skip an id, so a batch that cannot fit its limit stalls the chain at that id.
//!
//! These tests run the real compiled `zk_inbox.so` under the devnet inbox and settlement program ids, scan
//! enough ids and slots to meet the expensive addresses, and require the largest figure to fit the limit the
//! batcher ships with. Run with `--nocapture` to see the measured figures.
//!
//! Priority-fee cost of the limit: the fee is `limit x price`. At the 1,000 micro-lamport starting price a
//! 100,000 CU limit is 100 lamports, and at the 200,000 micro-lamport ceiling it is 20,000 lamports.

use rome_zk_batcher::channel::{DEFAULT_MAX_FRAME_BODY_LEN, FRAME_HEADER_LEN};
use rome_zk_batcher::config::Config;
use rome_zk_batcher::pipeline::plan_chunk;
use rome_zk_testkit::{cursor_account, root_account_with_authority};
use solana_program::pubkey::Pubkey;
use solana_program_test::ProgramTestContext;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
};
use solana_system_interface::program as system_program;

/// The devnet zk-inbox and zk-settlement program ids (`deploy/rollup/programs.devnet.json`).
const DEVNET_INBOX: &str = "28948c2Qt1QtG2XJA823ytzCfj7U5cNNm3FuM2KoXsAa";
const DEVNET_SETTLEMENT: &str = "8anSjJZu5vgfNbESPLoKudVBNEraDZASwKJnkDLTCaGo";
/// The public devnet smoke chain.
const CHAIN_ID: u64 = 6_343_061_215_884_205;
const LEAVES: u32 = 900;
const BATCH_IDS_SCANNED: u64 = 256;

/// The config a batcher gets from a file that sets nothing but the required keys.
fn shipped_config() -> Config {
    Config::from_toml_str(
        r#"
chain_id = 1
inbox_program_id = "11111111111111111111111111111111"
settlement_program_id = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
log_dir = "/var/lib/rome-zk/log"
rpc_url = "https://api.devnet.solana.com"
payer_key_path = "/home/rome/.config/solana/batcher.json"
cluster = "devnet"
"#,
    )
    .unwrap()
}

/// The limit a batcher gets from a config that sets nothing.
fn shipped_chunk_compute_unit_limit() -> u64 {
    shipped_config().chunk_compute_unit_limit as u64
}

/// The open-and-grow transaction's own limit, from a config that sets nothing.
fn shipped_open_compute_unit_limit() -> u64 {
    shipped_config().open_compute_unit_limit as u64
}

/// Batch ids whose address needed the most extra bump attempts found by a wider scan, pinned so the tail is
/// exercised on every run without scanning millions of ids.
const PINNED_TAIL_BATCH_IDS: [u64; 2] = [274_100, 1_895_697];
/// Each extra bump attempt on the batch address costs 4,500 CU in an open-and-grow transaction (three
/// derivations of 1,500 CU: `OpenBatch` and two `GrowBatch`).
const OPEN_CU_PER_EXTRA_ATTEMPT: u64 = 4_500;
const REQUIRED_EXTRA_ATTEMPTS_OF_HEADROOM: u64 = 40;

struct Fixture {
    ctx: ProgramTestContext,
    authority: Keypair,
    inbox: Pubkey,
    settlement: Pubkey,
}

async fn fixture() -> Fixture {
    let inbox: Pubkey = DEVNET_INBOX.parse().unwrap();
    let settlement: Pubkey = DEVNET_SETTLEMENT.parse().unwrap();
    let authority = Keypair::new();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", inbox)],
        // Leave the per-transaction budget at its default: every figure measured here sits far below it.
        false,
    );
    pt.add_account(
        zk_inbox_client::root_pda(&settlement, CHAIN_ID).0,
        root_account_with_authority(CHAIN_ID, &authority.pubkey(), settlement),
    );
    pt.add_account(
        zk_inbox_client::cursor_pda(&inbox, &settlement, CHAIN_ID).0,
        cursor_account(inbox, CHAIN_ID, 0),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 5_000_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let ctx = pt.start_with_context().await;
    Fixture {
        ctx,
        authority,
        inbox,
        settlement,
    }
}

impl Fixture {
    /// Points the cursor at `batch`, so the next `OpenBatch` may open exactly that id.
    fn set_cursor(&mut self, batch: u64) {
        self.ctx.set_account(
            &zk_inbox_client::cursor_pda(&self.inbox, &self.settlement, CHAIN_ID).0,
            &cursor_account(self.inbox, CHAIN_ID, batch).into(),
        );
    }

    /// One open-and-grow transaction for `batch`, exactly the instruction list the batcher sends. Returns its CU.
    async fn open_and_grow(&mut self, batch: u64) -> u64 {
        self.set_cursor(batch);
        let ixs = zk_inbox_client::open_and_grow_batch_ixs(
            &self.inbox,
            &self.authority.pubkey(),
            CHAIN_ID,
            batch,
            LEAVES,
            &self.settlement,
        );
        let (result, cu, _) =
            rome_zk_testkit::send_measuring_cu(&mut self.ctx, &ixs, &self.authority, &[]).await;
        result.unwrap_or_else(|e| panic!("open-and-grow for batch {batch} failed: {e:?}"));
        cu
    }
}

fn summarize(name: &str, mut v: Vec<(u64, u64)>) -> u64 {
    v.sort_by_key(|&(_, cu)| cu);
    let n = v.len();
    let (worst_id, worst) = v[n - 1];
    eprintln!(
        "CU {name}: n {n} min {} median {} p95 {} max {worst} (at {worst_id})",
        v[0].1,
        v[n / 2].1,
        v[n * 95 / 100].1,
    );
    worst
}

/// The open-and-grow transaction for a 900-leaf batch, over 256 batch ids, must fit the shipped limit.
#[tokio::test]
async fn open_and_grow_at_900_leaves_fits_the_chunk_compute_unit_limit_for_every_scanned_batch_id()
{
    let limit = shipped_chunk_compute_unit_limit();
    let mut f = fixture().await;
    let mut measured = Vec::new();
    for batch in 0..BATCH_IDS_SCANNED {
        measured.push((batch, f.open_and_grow(batch).await));
    }
    let worst = summarize("open-and-grow (900 leaves)", measured.clone());
    let over: Vec<u64> = measured
        .iter()
        .filter(|&&(_, cu)| cu > limit)
        .map(|&(id, _)| id)
        .collect();
    assert!(
        worst <= limit,
        "open-and-grow measured {worst} CU, over chunk_compute_unit_limit ({limit}); {} of {} scanned batch ids \
         do not fit: {over:?}",
        over.len(),
        measured.len(),
    );
}

/// Open-and-grow runs under its own limit (`open_compute_unit_limit`), not the chunk limit. The pinned tail batch ids
/// must fit it, and the limit must leave room for at least 40 extra bump attempts (40 x 4,500 CU) above the cost of
/// a batch id that needs none.
#[tokio::test]
async fn open_and_grow_fits_its_own_limit_at_the_tail_batch_ids_with_forty_attempts_of_headroom() {
    let limit = shipped_open_compute_unit_limit();
    let mut f = fixture().await;
    // The zero-extra-attempt cost: the cheapest of the scanned ids.
    let mut scanned = Vec::new();
    for batch in 0..BATCH_IDS_SCANNED {
        scanned.push(f.open_and_grow(batch).await);
    }
    let zero_attempt_cost = *scanned.iter().min().unwrap();
    for id in PINNED_TAIL_BATCH_IDS {
        let cu = f.open_and_grow(id).await;
        eprintln!(
            "CU open-and-grow (900 leaves) at pinned tail batch id {id}: {cu} (limit {limit})"
        );
        assert!(
            cu <= limit,
            "open-and-grow at batch id {id} measured {cu} CU, over open_compute_unit_limit ({limit})"
        );
    }
    let required = REQUIRED_EXTRA_ATTEMPTS_OF_HEADROOM * OPEN_CU_PER_EXTRA_ATTEMPT;
    assert!(
        limit >= zero_attempt_cost + required,
        "open_compute_unit_limit ({limit}) leaves {} CU above the zero-attempt cost ({zero_attempt_cost}); \
         {REQUIRED_EXTRA_ATTEMPTS_OF_HEADROOM} extra attempts need {required}",
        limit.saturating_sub(zero_attempt_cost),
    );
}

/// A chunk transaction with a full-size body, over 900 chunk slots of one batch, must fit the shipped limit.
/// The batch is the scanned id whose own address needed the most bump attempts, so the figure includes the
/// batch derivation at its most expensive as well as each slot's own.
#[tokio::test]
async fn full_body_chunk_transaction_fits_the_chunk_compute_unit_limit_for_every_slot() {
    let limit = shipped_chunk_compute_unit_limit();
    let mut f = fixture().await;
    let batch = (0..BATCH_IDS_SCANNED)
        .min_by_key(|&b| zk_inbox_client::batch_pda(&f.inbox, &f.settlement, CHAIN_ID, b).1)
        .unwrap();
    eprintln!("chunk lane scanned in batch id {batch}");
    f.open_and_grow(batch).await;

    let payload = vec![0xA5u8; FRAME_HEADER_LEN + DEFAULT_MAX_FRAME_BODY_LEN];
    let mut measured = Vec::new();
    for idx in 0..LEAVES {
        let ixs = plan_chunk(
            &f.inbox,
            &f.authority.pubkey(),
            &f.settlement,
            CHAIN_ID,
            batch,
            idx,
            &payload,
        );
        let (result, cu, _) =
            rome_zk_testkit::send_measuring_cu(&mut f.ctx, &ixs, &f.authority, &[]).await;
        result.unwrap_or_else(|e| panic!("chunk slot {idx} failed: {e:?}"));
        measured.push((idx as u64, cu));
    }
    let worst = summarize(
        "chunk Open+Write+Seal+SealLeaf (full body)",
        measured.clone(),
    );
    let over: Vec<u64> = measured
        .iter()
        .filter(|&&(_, cu)| cu > limit)
        .map(|&(i, _)| i)
        .collect();
    assert!(
        worst <= limit,
        "a full-body chunk transaction measured {worst} CU, over chunk_compute_unit_limit ({limit}); {} of {} \
         slots do not fit: {over:?}",
        over.len(),
        measured.len(),
    );
}
