//! Prints the compute units of the inbox instructions that derive settlement-keyed addresses, one figure
//! per instruction, on the real compiled program: `InitBatchCursor`, `OpenBatch`, chunk `Open`, `Write`,
//! `Seal` and `SealLeaf`, over sixteen chain ids. Run with `--nocapture`. `FinalizeBatch(900)` and chunk `Close` are measured by
//! `accumulator.rs` and the batcher's `abandon_cu_limit.rs`.

use rome_zk_testkit::root_account_with_authority;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
};
use solana_system_interface::program as system_program;
use zk_inbox_client as client;

/// `Program <id> consumed <N> of <M> compute units` for every top-level instruction of `program_id`.
fn per_instruction_cu(logs: &Option<Vec<String>>, program_id: &Pubkey) -> Vec<u64> {
    let prefix = format!("Program {program_id} consumed ");
    logs.as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|l| {
            l.strip_prefix(&prefix)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

/// One run of the sequence for `chain_id`: `InitBatchCursor`, `OpenBatch`, then one chunk's `Open`, `Write`,
/// `Seal`, `SealLeaf`. Returns the six figures in that order.
async fn measure(chain_id: u64) -> [u64; 6] {
    let program_id = rome_zk_testkit::fixed_inbox_program_id();
    let settlement_program = rome_zk_testkit::fixed_settlement_program_id();
    let authority = Keypair::new();
    let mut pt = rome_zk_testkit::program_test(
        &[rome_zk_testkit::ProgramSpec::new("zk_inbox", program_id)],
        true,
    );
    pt.add_account(
        client::root_pda(&settlement_program, chain_id).0,
        root_account_with_authority(chain_id, &authority.pubkey(), settlement_program),
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 100_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let mut ctx = pt.start_with_context().await;

    let ix = client::init_batch_cursor_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        1,
        &settlement_program,
    );
    let (r, init_cu, _) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &authority, &[]).await;
    r.expect("InitBatchCursor");

    let ix = client::open_batch_ix(
        &program_id,
        &authority.pubkey(),
        chain_id,
        1,
        1,
        &settlement_program,
    );
    let (r, open_cu, _) =
        rome_zk_testkit::send_measuring_cu(&mut ctx, &[ix], &authority, &[]).await;
    r.expect("OpenBatch");

    let body = vec![7u8; 64];
    let ixs = vec![
        client::open_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            1,
            0,
            body.len() as u32,
        ),
        client::write_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            1,
            0,
            0,
            body.clone(),
        ),
        client::seal_chunk_ix(
            &program_id,
            &authority.pubkey(),
            &settlement_program,
            chain_id,
            1,
            0,
            body.len() as u32,
            client::chunk_body_hash(&body),
        ),
        client::seal_leaf_ix(&program_id, &settlement_program, chain_id, 1, 0),
    ];
    let (r, _cu, logs) = rome_zk_testkit::send_measuring_cu(&mut ctx, &ixs, &authority, &[]).await;
    r.expect("chunk Open+Write+Seal+SealLeaf");
    let per = per_instruction_cu(&logs, &program_id);
    assert_eq!(per.len(), 4);
    [init_cu, open_cu, per[0], per[1], per[2], per[3]]
}

/// A PDA derivation costs more for every bump it has to skip, so one chain id's figure mixes the seed cost
/// with that address's luck. This runs sixteen chain ids and prints the minimum, median and maximum.
#[tokio::test]
async fn print_inbox_instruction_cu() {
    let names = [
        "InitBatchCursor",
        "OpenBatch (1 leaf)",
        "chunk Open",
        "chunk Write",
        "chunk Seal",
        "chunk SealLeaf",
    ];
    let mut runs: Vec<[u64; 6]> = Vec::new();
    for chain_id in 1..=16u64 {
        runs.push(measure(chain_id).await);
    }
    for (i, name) in names.iter().enumerate() {
        let mut v: Vec<u64> = runs.iter().map(|r| r[i]).collect();
        v.sort_unstable();
        eprintln!(
            "CU {name}: min {} median {} max {} (16 chain ids)",
            v[0],
            v[v.len() / 2],
            v[v.len() - 1]
        );
    }
}
