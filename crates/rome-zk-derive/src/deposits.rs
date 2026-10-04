//! The deposits of a batch: verify the range its header names, and turn the records into each block's
//! withdrawals.
//!
//! A v3 batch header carries a deposit range `[from, to)` and the queue's hash-chain values before deposit
//! `from` and before deposit `to`. Derive trusts none of it until it has checked it against the records:
//!
//! 1. the records `[from, to)` are read at finalized commitment, under `exit_config.bridge_program`;
//! 2. the hash chain over them (`rome_zk_layouts::deposit::chain_through`, the one definition) runs from the
//!    header's `hash_from` to exactly its `hash_to`;
//! 3. the cumulative cursor the blocks carry (`rome_zk_channel::resolve_deposits_end`, the same function the
//!    guest uses) ends at `to`;
//! 4. each block's slice of the records becomes its withdrawals, every one built with
//!    `rome_zk_executor_api::deposit_withdrawal`.
//!
//! Any mismatch is [`PipelineError::Critical`]: a sequencer fault, never a skip. A v2 header, or a v3 header
//! with an empty range, is a batch without deposits: it reads no extra account, and a block that carries a
//! `deposits_end` in it is refused.

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::Address;
use rome_zk_channel::Block;
use rome_zk_layouts::batch::BatchDeposit;
use rome_zk_layouts::deposit::DepositRecord;
use solana_program::pubkey::Pubkey;

use crate::reader::{AccountReader, MAX_ACCOUNTS_PER_GET_MULTIPLE};
use crate::PipelineError;

fn critical(batch: u64, msg: impl std::fmt::Display) -> PipelineError {
    PipelineError::Critical(format!("batch {batch}: deposits: {msg}"))
}

/// Reads the records `[range.from, range.to)` of `chain_id` at finalized commitment and returns them in
/// index order. Refuses ([`PipelineError::Critical`]) an inverted range, a missing, undecodable or
/// misplaced `exit_config`, a missing, undecodable or wrongly indexed record, and a hash chain that does not
/// run from `range.hash_from` to `range.hash_to`. An empty range reads nothing, but its two hashes must
/// still be equal (the chain over no records is its starting value).
pub async fn read_records<R: AccountReader>(
    reader: &mut R,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    range: &BatchDeposit,
) -> Result<Vec<DepositRecord>, PipelineError> {
    if range.from > range.to {
        return Err(critical(
            batch,
            format!(
                "the header's range is inverted (from {} is above to {})",
                range.from, range.to
            ),
        ));
    }
    let mut records: Vec<DepositRecord> = Vec::new();
    if range.from < range.to {
        let bridge_program = bridge_program(reader, settlement_program, chain_id, batch).await?;
        let mut next = range.from;
        while next < range.to {
            // A page at a time, so a header that names an absurd range fails at the first missing record
            // instead of allocating for it.
            let page_end = range
                .to
                .min(next.saturating_add(MAX_ACCOUNTS_PER_GET_MULTIPLE as u64));
            let pdas: Vec<Pubkey> = (next..page_end)
                .map(|i| {
                    rome_zk_layouts::deposit_queue::deposit_record::pda(
                        &bridge_program,
                        &settlement_program.to_bytes(),
                        chain_id,
                        i,
                    )
                    .0
                })
                .collect();
            let datas = reader.get_multiple_account_data(&pdas).await?;
            for (k, data) in datas.into_iter().enumerate() {
                let index = next + k as u64;
                let pda = pdas[k];
                let Some(data) = data.filter(|d| !d.is_empty()) else {
                    return Err(critical(
                        batch,
                        format!(
                            "deposit record {index} is missing at {pda} (the header's range is [{}, {}))",
                            range.from, range.to
                        ),
                    ));
                };
                let f =
                    rome_zk_layouts::deposit_queue::deposit_record::read(&data).map_err(|e| {
                        critical(
                            batch,
                            format!("deposit record {index} at {pda} is undecodable: {e:?}"),
                        )
                    })?;
                if f.index != index {
                    return Err(critical(
                        batch,
                        format!(
                            "the account at deposit record {index}'s address {pda} reports index {}",
                            f.index
                        ),
                    ));
                }
                records.push(DepositRecord {
                    sender: f.sender,
                    recipient: f.recipient,
                    amount_gwei: f.amount_gwei,
                });
            }
            next = page_end;
        }
    }
    let sk = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
    let h_to = rome_zk_layouts::deposit::chain_through(
        &sk,
        &settlement_program.to_bytes(),
        chain_id,
        range.from,
        &range.hash_from,
        &records,
    );
    if h_to != range.hash_to {
        return Err(critical(
            batch,
            format!(
                "the hash chain over records [{}, {}) ends at {h_to:02x?}, but the header's hash_to is {:02x?}",
                range.from, range.to, range.hash_to
            ),
        ));
    }
    Ok(records)
}

/// `exit_config.bridge_program` for the chain, read at finalized commitment under the settlement program.
async fn bridge_program<R: AccountReader>(
    reader: &mut R,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Result<Pubkey, PipelineError> {
    let (pda, _) = rome_zk_layouts::exit::exit_config::pda(settlement_program, chain_id);
    let Some(data) = reader
        .get_account_data(pda)
        .await?
        .filter(|d| !d.is_empty())
    else {
        return Err(critical(
            batch,
            format!(
                "the header names deposits but exit_config is missing at {pda} for chain {chain_id}"
            ),
        ));
    };
    let cfg = rome_zk_layouts::exit::exit_config::read(&data)
        .map_err(|e| critical(batch, format!("exit_config at {pda} is undecodable: {e:?}")))?;
    if cfg.chain_id != chain_id {
        return Err(critical(
            batch,
            format!(
                "exit_config at the expected address for chain {chain_id} reports chain {}",
                cfg.chain_id
            ),
        ));
    }
    if cfg.bridge_program == [0u8; 32] {
        return Err(critical(
            batch,
            format!(
                "exit_config at {pda} has no bridge program set, so the records cannot be read"
            ),
        ));
    }
    Ok(Pubkey::new_from_array(cfg.bridge_program))
}

/// The cumulative deposit cursor at the end of each block, checked against the header: `Ok(None)` for a batch
/// without deposits (a v2 header, or a range with `from == to`), `Ok(Some(ends))` for one with deposits.
///
/// Refuses: a `deposits_end` in a batch whose range is empty or absent; a cursor the strict-increase rule
/// rejects; a cursor whose last value is not the header's `to`. No account is read here, so it runs before
/// [`read_records`].
pub fn cursor_ends(
    batch: u64,
    blocks: &[Block],
    range: Option<&BatchDeposit>,
) -> Result<Option<Vec<u64>>, PipelineError> {
    let (from, to) = range.map_or((0, 0), |r| (r.from, r.to));
    if from > to {
        return Err(critical(
            batch,
            format!("the header's range is inverted (from {from} is above to {to})"),
        ));
    }
    if from == to {
        if let Some(i) = blocks.iter().position(|b| b.deposits_end.is_some()) {
            return Err(critical(
                batch,
                format!("block {i} carries a deposits_end but the batch has no deposit range"),
            ));
        }
        return Ok(None);
    }
    let ends = rome_zk_channel::resolve_deposits_end(blocks, from)
        .map_err(|e| critical(batch, format!("the blocks' deposit cursor is invalid: {e}")))?;
    let last = ends.last().copied().unwrap_or(from);
    if last != to {
        return Err(critical(
            batch,
            format!(
                "the blocks' deposit cursor ends at {last}, but the header's range ends at {to}"
            ),
        ));
    }
    Ok(Some(ends))
}

/// Splits the verified records `[from, ...)` into each block's withdrawals: block `i` gets the deposits
/// from the previous cursor value up to `ends[i]`, each built with
/// `deposit_withdrawal(index, recipient, amount_gwei)`.
pub fn split_withdrawals(
    batch: u64,
    from: u64,
    ends: &[u64],
    records: &[DepositRecord],
) -> Result<Vec<Vec<Withdrawal>>, PipelineError> {
    let to = ends.last().copied().unwrap_or(from);
    if records.len() as u64 != to - from {
        return Err(critical(
            batch,
            format!(
                "{} records were read for the range [{from}, {to})",
                records.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(ends.len());
    let mut previous = from;
    for &end in ends {
        out.push(
            (previous..end)
                .map(|index| {
                    let r = &records[(index - from) as usize];
                    rome_zk_executor_api::deposit_withdrawal(
                        index,
                        Address::from(r.recipient),
                        r.amount_gwei,
                    )
                })
                .collect(),
        );
        previous = end;
    }
    Ok(out)
}

/// Every block's withdrawals for one batch, verified end to end (the module doc's four steps). One list per
/// block, empty for a block without deposits; all empty, with no account read, for a batch without a range.
pub async fn batch_withdrawals<R: AccountReader>(
    reader: &mut R,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    range: Option<&BatchDeposit>,
    blocks: &[Block],
) -> Result<Vec<Vec<Withdrawal>>, PipelineError> {
    let Some(ends) = cursor_ends(batch, blocks, range)? else {
        return Ok(vec![Vec::new(); blocks.len()]);
    };
    let range = range.expect("a cursor exists only for a batch with a range");
    let records = read_records(reader, settlement_program, chain_id, batch, range).await?;
    split_withdrawals(batch, range.from, &ends, &records)
}
