//! The batcher's deposit side: what it reads from the chain, and the one place a batch's deposit range is
//! worked out.
//!
//! A batch's range `[from, to)` is never passed in by a caller. `from` is the live cursor's `deposit_next`
//! (read once the previous batch is finalized or abandoned), and `to` is the end the batch's own posted
//! stream ends at ([`channel::resolve_deposits_end`] over the decoded frames). Both the normal path and the
//! startup resume go through [`plan_range`], so a restart cannot send a range the stream does not carry: a
//! finalized batch with the wrong range could not be proved, and `AbandonBatch` refuses a finalized batch.
//!
//! Also here: the bridge program (read from the chain's exit config), the queue's per-batch limit for the
//! grouper, and the one-off rent top-up a chain's v1 cursor needs before its first `FinalizeBatchV2`.

use solana_program::pubkey::Pubkey;

use crate::channel::{self, Block, Frame};
use crate::grouping::DepositCap;
use crate::pipeline::{BatchTarget, PipelineError};
use crate::resolve::{AccountOps, ResolveError};
use crate::sender::{SendTuning, Sender};

/// The data of the account at `pda`, counted only when `owner` owns it. An account owned by anyone else is
/// absent: this is the rule the program applies to the exit config and the queue, and a third party can put
/// a funded, empty, system-owned account at either address.
async fn read_if_owned_by<A: AccountOps>(
    accounts: &A,
    pda: &Pubkey,
    owner: &Pubkey,
) -> Result<Option<Vec<u8>>, PipelineError> {
    let Some(data) = accounts.get_account(pda).await? else {
        return Ok(None);
    };
    Ok((accounts.get_account_owner(pda).await?.as_ref() == Some(owner)).then_some(data))
}

/// The chain's deposit bridge program: `exit_config.bridge_program`, read at its PDA under the settlement
/// program. `None` when the chain has no exit config (an account the settlement program does not own counts
/// as none, as it does in the program) or its config names no bridge: such a chain finalizes an empty range.
pub async fn read_bridge_program<A: AccountOps>(
    accounts: &A,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<Option<Pubkey>, PipelineError> {
    let (pda, _) = zk_inbox_client::exit_config_pda(settlement_program, chain_id);
    let Some(data) = read_if_owned_by(accounts, &pda, settlement_program).await? else {
        return Ok(None);
    };
    let fields = rome_zk_layouts::exit::exit_config::read(&data)
        .map_err(|e| PipelineError::Deposits(format!("decoding exit_config at {pda}: {e:?}")))?;
    Ok((fields.bridge_program != [0u8; 32]).then(|| Pubkey::new_from_array(fields.bridge_program)))
}

/// The batch cursor for `chain_id`, decoded. A missing account is the named [`ResolveError::CursorMissing`].
pub async fn read_cursor<A: AccountOps>(
    accounts: &A,
    inbox_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<zk_inbox_client::BatchCursor, PipelineError> {
    let (pda, _) = zk_inbox_client::cursor_pda(inbox_program, settlement_program, chain_id);
    let data = accounts
        .get_account(&pda)
        .await?
        .ok_or(ResolveError::CursorMissing { chain_id })?;
    Ok(zk_inbox_client::decode_batch_cursor(&data)?)
}

/// The deposit queue, decoded. `None` when the queue account does not exist yet, or is not owned by the
/// bridge program (the program counts it as absent then).
async fn read_queue<A: AccountOps>(
    accounts: &A,
    bridge_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<Option<rome_zk_layouts::deposit_queue::deposit_queue::DepositQueueFields>, PipelineError>
{
    use rome_zk_layouts::deposit_queue::deposit_queue;
    let (pda, _) = deposit_queue::pda(bridge_program, &settlement_program.to_bytes(), chain_id);
    let Some(data) = read_if_owned_by(accounts, &pda, bridge_program).await? else {
        return Ok(None);
    };
    deposit_queue::read(&data)
        .map(Some)
        .map_err(|e| PipelineError::Deposits(format!("decoding deposit_queue at {pda}: {e:?}")))
}

/// The deposit queue's per-batch limit: the active `max_per_batch` and, while a proposal waits to activate,
/// the pending one. `None` when the queue account does not exist yet, or is not owned by the bridge program
/// (the program counts it as absent then).
pub async fn read_deposit_cap<A: AccountOps>(
    accounts: &A,
    bridge_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<Option<DepositCap>, PipelineError> {
    Ok(
        read_queue(accounts, bridge_program, settlement_program, chain_id)
            .await?
            .map(|queue| DepositCap {
                active: u64::from(queue.params.max_per_batch),
                pending: (queue.activation_slot != 0)
                    .then_some(u64::from(queue.pending.max_per_batch)),
            }),
    )
}

/// What the batcher reads about deposits at startup, once the open batches are finished: the cursor's
/// `deposit_next` (the index the first batch it builds takes from; 0 for a v1 cursor) and the queue's limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositStart {
    pub next: u64,
    pub cap: Option<DepositCap>,
}

/// Reads the [`DepositStart`] from the chain. A chain with no bridge or no queue gives `next` from the
/// cursor and no cap, which is exactly right for a log without deposits.
pub async fn read_deposit_start<A: AccountOps>(
    accounts: &A,
    inbox_program: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
) -> Result<DepositStart, PipelineError> {
    let cursor = read_cursor(accounts, inbox_program, settlement_program, chain_id).await?;
    let next = cursor.deposit.map_or(0, |d| d.next);
    let cap = match read_bridge_program(accounts, settlement_program, chain_id).await? {
        Some(bridge) => read_deposit_cap(accounts, &bridge, settlement_program, chain_id).await?,
        None => None,
    };
    Ok(DepositStart { next, cap })
}

/// Reassembles and decodes the stream the frames carry: the compressed bytes and the blocks in them.
pub fn decode_posted(frames: &[Frame]) -> Result<(Vec<u8>, Vec<Block>), PipelineError> {
    let compressed = channel::reassemble(frames)?;
    let blocks = channel::decode_stream(&compressed)?;
    Ok((compressed, blocks))
}

/// The end of the deposit range a decoded stream ends at, given the index it starts from.
pub fn stream_deposit_to(blocks: &[Block], from: u64) -> Result<u64, PipelineError> {
    let ends = channel::resolve_deposits_end(blocks, from)?;
    Ok(ends.last().copied().unwrap_or(from))
}

/// How far inside the inclusion deadline a deposit must still be for a batch that leaves it out to be
/// opened, in seconds.
pub const OPEN_MARGIN_SECS: i64 = 300;

/// Before `OpenBatch`: refuses a batch the program would refuse at finalize for leaving an overdue deposit
/// out, so no such batch is ever opened. The program measures a deposit's age at the batch's own open time,
/// so whether the batch can finalize is settled the moment it opens; one opened too late can only be
/// abandoned, which halts settlement. The batch covers `[from, to)` and `open_unix_ts` is the time it is about
/// to open at (the batcher's wall clock, as the cluster's clock is not known before the send).
///
/// The rule is the program's, with a margin: nothing is refused when the chain has no bridge or no queue,
/// when the range reaches the queue's end, or when it already holds `max_per_block` deposits; otherwise the
/// deposit at `to` must be younger than the inclusion deadline minus [`OPEN_MARGIN_SECS`]. The margin covers
/// the gap between this host's clock and the cluster's, and the time `OpenBatch` takes to land after the
/// check. The deadline used is the stricter (smaller) of the active one and, while a proposal waits to
/// activate, the pending one, so a batch is not opened that a proposal activating in the meantime would
/// leave unable to finalize.
pub async fn check_deadline_at_open<A: AccountOps>(
    accounts: &A,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
    from: u64,
    to: u64,
    open_unix_ts: i64,
) -> Result<(), PipelineError> {
    use rome_zk_layouts::deposit_queue::deposit_record;
    let Some(bridge) = read_bridge_program(accounts, settlement_program, chain_id).await? else {
        return Ok(());
    };
    let Some(queue) = read_queue(accounts, &bridge, settlement_program, chain_id).await? else {
        return Ok(());
    };
    let taken = to.saturating_sub(from);
    if to >= queue.count || taken >= u64::from(queue.params.max_per_block) {
        return Ok(());
    }
    let (pda, _) = deposit_record::pda(&bridge, &settlement_program.to_bytes(), chain_id, to);
    let data = read_if_owned_by(accounts, &pda, &bridge)
        .await?
        .ok_or_else(|| {
            PipelineError::Deposits(format!(
                "batch {batch}: deposit {to} is below the queue's count {} but its record at {pda} is missing",
                queue.count
            ))
        })?;
    let record = deposit_record::read(&data)
        .map_err(|e| PipelineError::Deposits(format!("decoding deposit_record at {pda}: {e:?}")))?;
    let age = open_unix_ts.saturating_sub(record.enqueue_unix_ts);
    let deadline = if queue.activation_slot != 0 {
        queue
            .params
            .inclusion_deadline_secs
            .min(queue.pending.inclusion_deadline_secs)
    } else {
        queue.params.inclusion_deadline_secs
    };
    if age >= i64::from(deadline) - OPEN_MARGIN_SECS {
        return Err(PipelineError::Deposits(format!(
            "batch {batch} was not opened: it takes deposits {from}..{to} and leaves deposit {to} out, which is {age} s old against an inclusion deadline of {deadline} s (a batch is not opened within {OPEN_MARGIN_SECS} s of it), so the inbox could refuse to finalize it"
        )));
    }
    Ok(())
}

/// What a completing `FinalizeBatchV2` is sent for one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedRange {
    /// The cursor's `deposit_next` the range starts at.
    pub from: u64,
    /// The end the posted stream carries.
    pub to: u64,
    /// The bridge program the queue and records are named under, `None` on a chain without one.
    pub bridge_program: Option<Pubkey>,
}

/// Works out the range to finalize `frames` under, and refuses before anything is sent when the posted
/// stream does not say what the batcher means to send.
///
/// - `from` is the live cursor's `deposit_next`, or 0 while the cursor is still v1. The caller reads it
///   only once every earlier batch is finalized or abandoned.
/// - The stream is decoded from the frames (which `verify_presealed_leaves` has bound to the sealed
///   leaves), and when the caller holds the blocks the frames were cut from (`expected`), the decoded
///   blocks must equal them, the fifth field included.
/// - A stream that carries deposits on a chain without a bridge is refused by name.
pub async fn plan_range<A: AccountOps>(
    accounts: &A,
    target: &BatchTarget,
    frames: &[Frame],
    expected: Option<&[Block]>,
) -> Result<PlannedRange, PipelineError> {
    let (compressed, blocks) = decode_posted(frames)?;
    if let Some(expected) = expected {
        crate::pipeline::re_derive_and_check(expected, &compressed)?;
    }
    let cursor = read_cursor(
        accounts,
        &target.program_id,
        &target.settlement_program,
        target.chain_id,
    )
    .await?;
    let from = cursor.deposit.map_or(0, |d| d.next);
    let to = stream_deposit_to(&blocks, from)?;
    let bridge_program =
        read_bridge_program(accounts, &target.settlement_program, target.chain_id).await?;
    if to != from && bridge_program.is_none() {
        return Err(PipelineError::Deposits(format!(
            "batch {}: the posted stream takes deposits {from}..{to}, but chain {} has no bridge program \
             in its exit config",
            target.batch, target.chain_id
        )));
    }
    Ok(PlannedRange {
        from,
        to,
        bridge_program,
    })
}

/// Before a chain's first `FinalizeBatchV2`: tops a v1 cursor up to the 69-byte rent minimum with one plain
/// system transfer from the payer. The instruction that grows the cursor has no payer, so the lamports must
/// already be there. Idempotent: it reads the cursor and its balance first and sends nothing when the cursor
/// is already v2 or already holds the minimum, so a restart or a second call never pays twice. Returns the
/// lamports sent.
pub async fn top_up_v1_cursor<S: Sender, A: AccountOps>(
    sender: &S,
    accounts: &A,
    target: &BatchTarget,
    tuning: SendTuning,
) -> Result<u64, PipelineError> {
    let cursor = read_cursor(
        accounts,
        &target.program_id,
        &target.settlement_program,
        target.chain_id,
    )
    .await?;
    if cursor.deposit.is_some() {
        return Ok(0);
    }
    let (cursor_pda, _) = zk_inbox_client::cursor_pda(
        &target.program_id,
        &target.settlement_program,
        target.chain_id,
    );
    let shortfall = accounts
        .rent_shortfall(&cursor_pda, rome_zk_layouts::cursor::LEN_V2)
        .await?
        .unwrap_or(0);
    if shortfall == 0 {
        return Ok(0);
    }
    let transfer =
        solana_system_interface::instruction::transfer(&target.payer, &cursor_pda, shortfall);
    sender
        .send_and_confirm(std::slice::from_ref(&transfer), tuning)
        .await?;
    tracing::info!(
        "topped the v1 batch cursor of chain {} up by {shortfall} lamports before its first FinalizeBatchV2",
        target.chain_id
    );
    Ok(shortfall)
}
