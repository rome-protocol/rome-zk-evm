//! Wires live on-chain reads to a resume decision for one chain (the stateless resume over the chain's
//! `batch_cursor`). [`resolve_batch_id`] here is what `bin/rome-zk-batcher.rs` calls directly.
//!
//! Two behaviours worth knowing:
//! - **Rerun detection:** the cursor path alone (`resolve_via_cursor`) never checked whether the batch
//!   immediately behind `next_batch` was already posted with exactly this run's own content — a rerun of
//!   `--once` on the same log would resolve `PostUnder(next_batch)` and post the same blocks again under a
//!   *new* id. Now, whenever `next_batch > 0`, `next_batch - 1`'s state is read first: finalized + this
//!   run's re-derived `acc` matches → `AlreadyPosted` (nothing left to send); finalized + mismatch →
//!   proceed (this run's content is genuinely new); open-not-finalized → warn and proceed (an earlier
//!   attempt at that id may still be live; this run cannot safely decide anything about it, only about
//!   its own `next_batch`).
//! - **No scan fallback:** the pre-cursor id-by-id scan fallback (`resolve_via_scan`, formerly here) is
//!   dead-on-arrival against the current, 5-account `OpenBatch` — a chain with no `batch_cursor` account
//!   cannot ever complete a real `OpenBatch` (the instruction requires the cursor PDA to exist), yet the
//!   old scan could still send real `AbandonBatch` transactions along the way before failing anyway. A
//!   missing cursor is now a hard, named error that tells the operator to run `InitBatchCursor` instead.
//!
//! Two more properties hold on the surviving cursor path:
//! - **RPC-error propagation:** an account read that fails for a real reason (RPC/network error) is never
//!   silently folded into `Missing` — only a genuine "no such account" result is (`AccountOps::get_account`
//!   returns `Ok(None)` for that, `Err` for everything else).
//! - **Content re-verification:** a batch id already finalized on chain is re-verified against this run's
//!   own re-derived frames (`pipeline::verify_acc`) before being treated as anything, per the rerun check above.

use crate::channel;
use crate::pipeline::{self, PipelineError};
use crate::resume::BatchAccountState;
use solana_program::pubkey::Pubkey;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// `message` is `describe_rpc_error(&source)` computed at construction time —
    /// `source`'s own `Display` (reqwest's, via `ClientError`) can carry a request URL with a secret (an
    /// API key in its query string) and must never reach a log line unredacted.
    #[error("reading account {pubkey}: {message}")]
    AccountRead {
        pubkey: Pubkey,
        message: String,
        #[source]
        source: Box<solana_client::client_error::ClientError>,
    },
    #[error("decoding batch account at batch {batch}: {source}")]
    BatchAccountDecode {
        batch: u64,
        #[source]
        source: zk_inbox_client::DecodeError,
    },
    #[error("batch {batch}'s account vanished between reads (raced by a concurrent process?)")]
    BatchAccountVanished { batch: u64 },
    #[error("decoding batch_cursor account for chain {chain_id}: {source}")]
    CursorAccountDecode {
        chain_id: u64,
        #[source]
        source: zk_inbox_client::DecodeError,
    },
    #[error(
        "batch_cursor.next_batch = {batch}, but a chunk PDA under it already exists — this should be \
         unreachable once InitBatchCursor is in force (OpenBatch cannot have run at {batch} without \
         already incrementing the cursor past it), so this is treated as a hard error rather than \
         silently advancing past the cursor's own invariant"
    )]
    CursorBatchAlreadyTouched { batch: u64 },
    #[error(
        "chain {chain_id} has no batch_cursor account — OpenBatch on this program requires one (it is \
         part of every OpenBatch's account list), so there is no safe id this process could resolve to. \
         Run InitBatchCursor for this chain first, at next_batch = root.head_pending_batch + 1 (zk-inbox-client's \
         examples/init_cursor takes --next-batch; examples/find_max_batch_id reads the root and prints that value). \
         Never above head_pending_batch + 1: settlement posts only that id next, so a higher cursor halts the chain"
    )]
    CursorMissing { chain_id: u64 },
    /// Chain-anchor resolution: a paged `getMultipleAccounts` call came back
    /// short — a provider truncating below the page size (e.g. a rate limit) — never silently
    /// concatenated, which would shift every later index in the caller's own account-index space (mirrors
    /// `rome-zk-derive::reader::RpcAccountReader::get_multiple_account_data`'s own short-page guard).
    #[error("getMultipleAccounts: {got} of {expected} entries in one page")]
    ShortPage { expected: usize, got: usize },
    /// Exactly one batcher process per chain authority. `expected_next_batch` is this
    /// run's own belief about the cursor (read once, after the startup abandon, and advanced by one after
    /// every successful post) — if the on-chain cursor no longer agrees, some other process (another
    /// instance holding the same authority key) posted under it. Never treated as `PostUnder`: the content-
    /// equality `AlreadyPosted` check alone cannot tell "a rerun of this exact run" apart from "a second
    /// writer that happened to post the same content", so the cursor position itself is the guard.
    #[error(
        "another writer advanced the cursor — exiting; restart re-anchors (this run expected \
         batch_cursor.next_batch={expected}, on-chain it is {got} — exactly one batcher process per chain \
         authority; a second instance holding the same key is destructive)"
    )]
    CursorAdvanced { expected: u64, got: u64 },
}

/// The account reads [`resolve_batch_id`] needs, and nothing else — modeled as a trait (mirrors
/// `sender::RpcOps`) so this module's own tests drive the exact production decision loop against a
/// scripted fake instead of a real network. [`solana_client::nonblocking::rpc_client::RpcClient`]
/// implements it for production use.
pub trait AccountOps: Send + Sync {
    /// `Ok(None)` = the account does not exist. Any other outcome is a genuine read failure and must
    /// propagate as `Err` — never be silently folded into "does not exist".
    fn get_account(
        &self,
        pubkey: &Pubkey,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, ResolveError>> + Send;

    /// One entry per input pubkey, in order — `true` iff some account exists there. Chunking to the RPC
    /// node's own `getMultipleAccounts` cap is this trait impl's job, not the caller's.
    fn accounts_exist(
        &self,
        pubkeys: &[Pubkey],
    ) -> impl std::future::Future<Output = Result<Vec<bool>, ResolveError>> + Send;

    /// Chain-anchor resolution: the data-returning sibling of
    /// [`Self::accounts_exist`] — one entry per input pubkey, in order, `None` where no account exists.
    /// Paging to the RPC node's own `getMultipleAccounts` cap is this trait impl's job (mirrors
    /// `rome-zk-derive::reader::AccountReader::get_multiple_account_data` — this crate does not depend on
    /// `rome-zk-derive` normally, so the same shape is repeated here rather than shared, per this crate's
    /// own "no normal dependency on rome-zk-derive" rule).
    ///
    /// Default implementation: one [`Self::get_account`] call per key, in order — correct for every
    /// backend with no batched read of its own (in particular this crate's own test fakes).
    /// [`RpcClient`](solana_client::nonblocking::rpc_client::RpcClient)'s impl below overrides this with
    /// the real, paged `getMultipleAccounts`.
    fn get_multiple_account_data(
        &self,
        pubkeys: &[Pubkey],
    ) -> impl std::future::Future<Output = Result<Vec<Option<Vec<u8>>>, ResolveError>> + Send {
        async move {
            let mut out = Vec::with_capacity(pubkeys.len());
            for pubkey in pubkeys {
                out.push(self.get_account(pubkey).await?);
            }
            Ok(out)
        }
    }
}

impl AccountOps for solana_client::nonblocking::rpc_client::RpcClient {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
        let resp = Self::get_account_with_commitment(self, pubkey, self.commitment())
            .await
            .map_err(|e| ResolveError::AccountRead {
                pubkey: *pubkey,
                message: rome_zk_solana_sender::describe_rpc_error(&e),
                source: Box::new(e),
            })?;
        Ok(resp.value.map(|a| a.data))
    }

    async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
        let mut out = Vec::with_capacity(pubkeys.len());
        for chunk in pubkeys.chunks(crate::sender::MAX_MULTIPLE_ACCOUNTS) {
            let accounts = Self::get_multiple_accounts(self, chunk)
                .await
                .map_err(|e| ResolveError::AccountRead {
                    pubkey: chunk[0],
                    message: rome_zk_solana_sender::describe_rpc_error(&e),
                    source: Box::new(e),
                })?;
            out.extend(accounts.into_iter().map(|a| a.is_some()));
        }
        Ok(out)
    }

    async fn get_multiple_account_data(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, ResolveError> {
        let mut out = Vec::with_capacity(pubkeys.len());
        for chunk in pubkeys.chunks(crate::sender::MAX_MULTIPLE_ACCOUNTS) {
            let accounts = Self::get_multiple_accounts(self, chunk)
                .await
                .map_err(|e| ResolveError::AccountRead {
                    pubkey: chunk[0],
                    message: rome_zk_solana_sender::describe_rpc_error(&e),
                    source: Box::new(e),
                })?;
            if accounts.len() != chunk.len() {
                return Err(ResolveError::ShortPage {
                    expected: chunk.len(),
                    got: accounts.len(),
                });
            }
            out.extend(accounts.into_iter().map(|a| a.map(|a| a.data)));
        }
        Ok(out)
    }
}

/// What [`resolve_batch_id`] decided this process should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Post this run's frames under this (never-touched, never-opened) batch id.
    PostUnder(u64),
    /// This exact content (`verify_acc` matches) is already finalized on chain under this id — nothing
    /// left to send; this run is done.
    AlreadyPosted(u64),
}

/// The one home both the chain-anchor walk (`anchor.rs`) and the startup recovery
/// (`recover::resume_open_batches`) page their batch-account probes through. It
/// mirrors the RPC node's own `getMultipleAccounts` cap (100), so a page here is exactly one real RPC round
/// trip. Reading one batch account at a time (`getAccountInfo` per id) would be far slower: while
/// `head_final_batch` is still 0 on a chain that has not settled, the startup recovery's pending window can span every
/// batch the chain has ever opened — thousands of ids, and thousands of sequential round trips, against a
/// rate-limited RPC on every restart.
pub(crate) const PROBE_PAGE_SIZE: usize = 100;

/// Decodes one already-fetched batch account's raw bytes into a [`BatchAccountState`] — the per-entry
/// decode both [`probe_batch_state`] (one account) and [`probe_batch_states_paged`] (many, paged) share,
/// and that `anchor.rs`'s own walk also drives (wrapping the result into its own `AnchorError` instead of
/// `ResolveError` — see that module's `decode_probe`).
pub(crate) fn decode_batch_probe(
    data: Option<Vec<u8>>,
    batch: u64,
) -> Result<BatchAccountState, ResolveError> {
    match data {
        None => Ok(BatchAccountState::Missing),
        Some(data) => match zk_inbox_client::decode_batch_account(&data) {
            Err(source) => Err(ResolveError::BatchAccountDecode { batch, source }),
            Ok(d) if d.finalized => Ok(BatchAccountState::Finalized),
            Ok(d) => Ok(BatchAccountState::OpenNotFinalized {
                leaves_present: d.leaves_present,
                expected_count: d.expected_count,
            }),
        },
    }
}

pub(crate) async fn probe_batch_state<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Result<BatchAccountState, ResolveError> {
    let (pda, _) =
        zk_inbox_client::batch_pda(inbox_program_id, settlement_program_id, chain_id, batch);
    let data = accounts.get_account(&pda).await?;
    decode_batch_probe(data, batch)
}

/// Probes every id in `ids` (any order — the startup recovery passes them ascending, ready-to-iterate),
/// paged at [`PROBE_PAGE_SIZE`] per [`AccountOps::get_multiple_account_data`] call — never one
/// `get_account` per id. Returns one [`BatchAccountState`] per input id, in the same order.
pub(crate) async fn probe_batch_states_paged<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    ids: &[u64],
) -> Result<Vec<BatchAccountState>, ResolveError> {
    let mut out = Vec::with_capacity(ids.len());
    for page in ids.chunks(PROBE_PAGE_SIZE) {
        let pdas: Vec<Pubkey> = page
            .iter()
            .map(|&b| {
                zk_inbox_client::batch_pda(inbox_program_id, settlement_program_id, chain_id, b).0
            })
            .collect();
        let datas = accounts.get_multiple_account_data(&pdas).await?;
        // The guard lives here, where the length is relied on — not only inside one backend's impl: a
        // short page would otherwise zip-truncate and silently drop the tail ids of the scan.
        if datas.len() != pdas.len() {
            return Err(ResolveError::ShortPage {
                expected: pdas.len(),
                got: datas.len(),
            });
        }
        for (&b, data) in page.iter().zip(datas) {
            out.push(decode_batch_probe(data, b)?);
        }
    }
    Ok(out)
}

/// Before treating `batch` as safe to `OpenBatch` under, checks whether any chunk PDA in `0..frame_count`
/// already exists — leftover from an earlier attempt at this same id (chunk PDAs are never touched by
/// `AbandonBatch`, only the batch account is).
async fn chunk_range_touched<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    frame_count: u32,
) -> Result<bool, ResolveError> {
    if frame_count == 0 {
        return Ok(false);
    }
    let pdas: Vec<Pubkey> = (0..frame_count)
        .map(|idx| {
            zk_inbox_client::chunk_pda(
                inbox_program_id,
                settlement_program_id,
                chain_id,
                batch,
                idx,
            )
            .0
        })
        .collect();
    Ok(accounts
        .accounts_exist(&pdas)
        .await?
        .into_iter()
        .any(|exists| exists))
}

enum FinalizedDecision {
    Matches,
    Mismatch(PipelineError),
}

/// Re-derives this run's own frames for `batch` and compares them to what is actually on chain
/// (`pipeline::verify_acc`) — called only for an id already known to be `Finalized`.
async fn decide_finalized<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    compressed: &[u8],
    max_frame_body_len: usize,
) -> Result<FinalizedDecision, ResolveError> {
    let (pda, _) =
        zk_inbox_client::batch_pda(inbox_program_id, settlement_program_id, chain_id, batch);
    let data = accounts
        .get_account(&pda)
        .await?
        .ok_or(ResolveError::BatchAccountVanished { batch })?;
    let decoded = zk_inbox_client::decode_batch_account(&data)
        .map_err(|source| ResolveError::BatchAccountDecode { batch, source })?;
    let frames = channel::cut_frames(chain_id, batch, compressed, max_frame_body_len);
    match pipeline::verify_acc(&decoded, &frames) {
        Ok(_) => Ok(FinalizedDecision::Matches),
        Err(source) => Ok(FinalizedDecision::Mismatch(source)),
    }
}

/// Stateless resume, driven live against on-chain state, over the chain's `batch_cursor`
/// PDA — the only supported path: a chain without a cursor yet
/// cannot ever complete a real `OpenBatch` (see [`ResolveError::CursorMissing`]).
///
/// **One batcher per chain authority:** `expected_next_batch` is this run's own
/// belief about the cursor — read once at startup (after the half-written-batch recovery,
/// `pipeline::startup_recover`) and advanced by the caller after every successful post.
/// If the on-chain cursor has moved past it, another writer (a second instance holding this chain's
/// authority key) posted under it — refused by name ([`ResolveError::CursorAdvanced`]) rather than
/// treated as this run's own `PostUnder`, which would double-post and abandon the peer's in-flight batch.
pub async fn resolve_batch_id<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    compressed: &[u8],
    max_frame_body_len: usize,
    expected_next_batch: u64,
) -> Result<ResolveOutcome, ResolveError> {
    let (cursor_pda, _) =
        zk_inbox_client::cursor_pda(inbox_program_id, settlement_program_id, chain_id);
    let data = accounts
        .get_account(&cursor_pda)
        .await?
        .ok_or(ResolveError::CursorMissing { chain_id })?;
    let cursor = zk_inbox_client::decode_batch_cursor(&data)
        .map_err(|source| ResolveError::CursorAccountDecode { chain_id, source })?;
    if cursor.next_batch != expected_next_batch {
        return Err(ResolveError::CursorAdvanced {
            expected: expected_next_batch,
            got: cursor.next_batch,
        });
    }
    resolve_via_cursor(
        accounts,
        inbox_program_id,
        settlement_program_id,
        chain_id,
        cursor.next_batch,
        compressed,
        max_frame_body_len,
    )
    .await
}

/// Reads the chain's `batch_cursor.next_batch` directly — what a run seeds `expected_next_batch` from,
/// once, right after the startup half-written-batch recovery: finishing a batch never
/// touches the cursor (only `OpenBatch` advances it), so
/// this is simply "whatever the cursor says right now", read through the same [`AccountOps`] seam.
pub async fn read_cursor_next_batch<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
) -> Result<u64, ResolveError> {
    let (cursor_pda, _) =
        zk_inbox_client::cursor_pda(inbox_program_id, settlement_program_id, chain_id);
    let data = accounts
        .get_account(&cursor_pda)
        .await?
        .ok_or(ResolveError::CursorMissing { chain_id })?;
    let cursor = zk_inbox_client::decode_batch_cursor(&data)
        .map_err(|source| ResolveError::CursorAccountDecode { chain_id, source })?;
    Ok(cursor.next_batch)
}

/// The cursor-based resume path: `next_batch` is already, by the on-chain
/// program's own invariant, an id nothing has ever opened — so it is always safe to `OpenBatch` there,
/// *unless* this exact content was already posted under the immediately preceding id (a rerun
/// of `--once` on the same log must not double-post).
async fn resolve_via_cursor<A: AccountOps>(
    accounts: &A,
    inbox_program_id: &Pubkey,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    next_batch: u64,
    compressed: &[u8],
    max_frame_body_len: usize,
) -> Result<ResolveOutcome, ResolveError> {
    if next_batch > 0 {
        let prev = next_batch - 1;
        match probe_batch_state(
            accounts,
            inbox_program_id,
            settlement_program_id,
            chain_id,
            prev,
        )
        .await?
        {
            BatchAccountState::Finalized => {
                match decide_finalized(
                    accounts,
                    inbox_program_id,
                    settlement_program_id,
                    chain_id,
                    prev,
                    compressed,
                    max_frame_body_len,
                )
                .await?
                {
                    FinalizedDecision::Matches => return Ok(ResolveOutcome::AlreadyPosted(prev)),
                    // This run's content genuinely differs from what's already posted at `prev` — it is
                    // new content, not a rerun; proceed to open `next_batch` under the cursor below.
                    FinalizedDecision::Mismatch(reason) => {
                        tracing::debug!(
                            "chain {chain_id}: batch {prev} is finalized with content different from \
                             this run's own frames ({reason}) — treating this run as new content, not a \
                             rerun; proceeding to open batch {next_batch}"
                        );
                    }
                }
            }
            BatchAccountState::OpenNotFinalized { .. } => {
                // A `prev` left open by a crashed run is finished under the same id by the startup
                // recovery (`pipeline::startup_recover`) at the next start; here, in a live
                // run, it is this run's own batch still finalizing. Under the posting window this is the NORMAL
                // steady state — our own batch `prev` is still finalizing while `next_batch` opens — so it is
                // debug, not warn: a warn on every batch would drown the real ones (recovery, CU budget).
                tracing::debug!(
                    "chain {chain_id}: batch {prev} (cursor.next_batch - 1) exists but is not yet \
                     finalized — its content cannot be safely compared to this run's own frames \
                     (a live process — normally this one, under the posting window — is still posting \
                     it); proceeding to open batch {next_batch} under the cursor"
                );
            }
            // `prev` has no account (never opened, or abandoned by hand; or, on a fresh chain with next_batch == 1, this branch cannot
            // occur) — nothing to compare this run's content against; proceed.
            BatchAccountState::Missing => {}
        }
    }

    let frames = channel::cut_frames(chain_id, next_batch, compressed, max_frame_body_len);
    let touched = chunk_range_touched(
        accounts,
        inbox_program_id,
        settlement_program_id,
        chain_id,
        next_batch,
        frames.len() as u32,
    )
    .await?;
    if touched {
        return Err(ResolveError::CursorBatchAlreadyTouched { batch: next_batch });
    }
    Ok(ResolveOutcome::PostUnder(next_batch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    const PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);
    const SETTLEMENT_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);
    const CHAIN_ID: u64 = 200_198;

    #[derive(Default)]
    struct FakeChainState {
        accounts: HashMap<Pubkey, Vec<u8>>,
        read_errors: HashSet<Pubkey>,
    }

    #[derive(Clone, Default)]
    struct FakeChain(Arc<Mutex<FakeChainState>>);

    impl FakeChain {
        fn set_account(&self, pubkey: Pubkey, data: Vec<u8>) {
            self.0.lock().unwrap().accounts.insert(pubkey, data);
        }

        fn fail_reads_for(&self, pubkey: Pubkey) {
            self.0.lock().unwrap().read_errors.insert(pubkey);
        }
    }

    fn fake_rpc_error() -> Box<solana_client::client_error::ClientError> {
        Box::new(
            solana_client::rpc_request::RpcError::ForUser("fake RPC failure".to_string()).into(),
        )
    }

    impl AccountOps for FakeChain {
        async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
            let st = self.0.lock().unwrap();
            if st.read_errors.contains(pubkey) {
                return Err(ResolveError::AccountRead {
                    pubkey: *pubkey,
                    message: "fake RPC failure".to_string(),
                    source: fake_rpc_error(),
                });
            }
            Ok(st.accounts.get(pubkey).cloned())
        }

        async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
            let st = self.0.lock().unwrap();
            for p in pubkeys {
                if st.read_errors.contains(p) {
                    return Err(ResolveError::AccountRead {
                        pubkey: *p,
                        message: "fake RPC failure".to_string(),
                        source: fake_rpc_error(),
                    });
                }
            }
            Ok(pubkeys
                .iter()
                .map(|p| st.accounts.contains_key(p))
                .collect())
        }
    }

    fn cursor_bytes(chain_id: u64, next_batch: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::cursor::LEN];
        d[rome_zk_layouts::cursor::OFF_MAGIC..rome_zk_layouts::cursor::OFF_MAGIC + 4]
            .copy_from_slice(&rome_zk_layouts::cursor::MAGIC.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_VERSION] = rome_zk_layouts::cursor::VERSION;
        d[rome_zk_layouts::cursor::OFF_CHAIN_ID..rome_zk_layouts::cursor::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::cursor::OFF_NEXT_BATCH..rome_zk_layouts::cursor::OFF_NEXT_BATCH + 8]
            .copy_from_slice(&next_batch.to_le_bytes());
        d
    }

    fn open_not_finalized_bytes(expected_count: u32, leaves_present: u32) -> Vec<u8> {
        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(
                rome_zk_layouts::batch::VERSION,
                expected_count
            )
            .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&expected_count.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
            ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
            .copy_from_slice(&leaves_present.to_le_bytes());
        d
    }

    fn finalized_matching_batch_bytes(chain_id: u64, batch: u64, compressed: &[u8]) -> Vec<u8> {
        let frames = channel::cut_frames(chain_id, batch, compressed, 3200);
        let chunk_hashes: Vec<[u8; 32]> = frames
            .iter()
            .map(|f| solana_program::keccak::hashv(&[&f.to_bytes()]).to_bytes())
            .collect();
        let (_, _, acc) = zk_inbox_client::reference_commitment(chain_id, batch, 0, &chunk_hashes);

        let mut d = vec![
            0u8;
            rome_zk_layouts::batch::account_len_for(
                rome_zk_layouts::batch::VERSION,
                frames.len() as u32
            )
            .unwrap()
        ];
        d[0..4].copy_from_slice(&rome_zk_layouts::batch::MAGIC.to_le_bytes());
        d[4] = rome_zk_layouts::batch::VERSION;
        d[rome_zk_layouts::batch::OFF_CHAIN_ID..rome_zk_layouts::batch::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_BATCH..rome_zk_layouts::batch::OFF_BATCH + 8]
            .copy_from_slice(&batch.to_le_bytes());
        d[rome_zk_layouts::batch::OFF_EXPECTED_COUNT
            ..rome_zk_layouts::batch::OFF_EXPECTED_COUNT + 4]
            .copy_from_slice(&(frames.len() as u32).to_le_bytes());
        d[rome_zk_layouts::batch::OFF_LEAVES_PRESENT
            ..rome_zk_layouts::batch::OFF_LEAVES_PRESENT + 4]
            .copy_from_slice(&(frames.len() as u32).to_le_bytes());
        d[rome_zk_layouts::batch::OFF_FINALIZED] = 1;
        d[rome_zk_layouts::batch::OFF_ACC..rome_zk_layouts::batch::OFF_ACC + 32]
            .copy_from_slice(&acc);
        d
    }

    // ===== Cursor absent is a hard error, never a scan =====

    #[tokio::test]
    async fn a_chain_without_a_cursor_is_a_hard_cursor_missing_error() {
        let chain = FakeChain::default();
        let err = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"hello world",
            32,
            0,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ResolveError::CursorMissing { chain_id } if chain_id == CHAIN_ID
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("root.head_pending_batch + 1") && msg.contains("init_cursor"),
            "the operator is told the cursor value to use: {msg}"
        );
    }

    // ===== The cursor-based resume path =====

    #[tokio::test]
    async fn a_chain_with_a_cursor_at_zero_resolves_directly_to_post_under_zero() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 0));

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"hello world",
            32,
            0,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(0));
    }

    #[tokio::test]
    async fn a_chain_with_a_cursor_resolves_directly_to_next_batch_without_scanning() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 5));
        // batch 4 (next_batch - 1) is finalized with *different* content than this run's own — proceed.
        let (batch4, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 4);
        chain.set_account(
            batch4,
            finalized_matching_batch_bytes(CHAIN_ID, 4, b"unrelated content"),
        );

        // Ids 0..3 are never seeded at all — the old scan would have needed each of them to exist or
        // read Missing; the cursor path never even looks at them.
        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"hello world",
            32,
            5,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(5));
    }

    #[tokio::test]
    async fn a_touched_chunk_range_under_the_cursors_next_batch_is_a_hard_error() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 3));
        let (chunk0, _) = zk_inbox_client::chunk_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 3, 0);
        chain.set_account(chunk0, vec![1, 2, 3]);

        let err = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            3,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ResolveError::CursorBatchAlreadyTouched { batch: 3 }
        ));
    }

    #[tokio::test]
    async fn a_real_rpc_failure_reading_the_cursor_propagates() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.fail_reads_for(cursor_pda);

        let err = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            0,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ResolveError::AccountRead { pubkey, .. } if pubkey == cursor_pda));
    }

    // ===== Exactly one batcher per chain authority =====

    /// The on-chain cursor has moved past this run's own `expected_next_batch`
    /// (a second writer holding the same authority key posted in between) — refused by name, never
    /// resolved as this run's own `PostUnder`. This is the repro shape: run A's own belief
    /// (`expected_next_batch`) is stale the moment run B posts.
    #[tokio::test]
    async fn a_cursor_that_moved_since_this_runs_expected_next_batch_refuses_by_name() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        // The on-chain cursor already sits at 6 (another writer posted batch 5 after this run last read
        // the cursor at startup and formed its own expectation of 5).
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 6));

        let err = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            5,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                ResolveError::CursorAdvanced {
                    expected: 5,
                    got: 6
                }
            ),
            "{err:?}"
        );
    }

    /// The matching positive case: when the on-chain cursor still agrees with `expected_next_batch`,
    /// resolution proceeds exactly as before (no second writer in the picture).
    #[tokio::test]
    async fn a_cursor_matching_expected_next_batch_proceeds_normally() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 5));

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            5,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(5));
    }

    #[tokio::test]
    async fn read_cursor_next_batch_reads_the_current_on_chain_value() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 7));
        let n = read_cursor_next_batch(&chain, &PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID)
            .await
            .unwrap();
        assert_eq!(n, 7);
    }

    // ===== The prev-batch AlreadyPosted check =====

    /// Core repro: a rerun of `--once` on the same log after batch 0 was already finalized
    /// with exactly this run's content — the cursor now sits at `next_batch = 1`. Before the fix, this
    /// resolved `PostUnder(1)` and posted the same blocks again under a new id; the test requires
    /// `AlreadyPosted`.
    #[tokio::test]
    async fn a_rerun_over_an_already_posted_prev_batch_resolves_to_already_posted_not_a_new_id() {
        let chain = FakeChain::default();
        let compressed = b"same content every time";
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (batch0, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        chain.set_account(
            batch0,
            finalized_matching_batch_bytes(CHAIN_ID, 0, compressed),
        );

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            compressed,
            3200,
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::AlreadyPosted(0),
            "must never double-post batch 0's content under a new id"
        );
    }

    /// The prev batch is finalized but with genuinely different content (e.g. this run really does hold
    /// new blocks) — proceed to open `next_batch`, never refuse and never treat it as already posted.
    #[tokio::test]
    async fn a_finalized_prev_batch_with_different_content_proceeds_to_the_next_id() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (batch0, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        chain.set_account(
            batch0,
            finalized_matching_batch_bytes(CHAIN_ID, 0, b"yesterday's content"),
        );

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"today's content",
            3200,
            1,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(1));
    }

    /// The prev batch exists but is not yet finalized (an earlier attempt may still be live, or died
    /// mid-post) — this run cannot safely compare content against it, so it only warns and still proceeds
    /// to open `next_batch` (which the cursor's own invariant already guarantees is untouched).
    #[tokio::test]
    async fn a_not_yet_finalized_prev_batch_warns_and_still_proceeds() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        let (batch0, _) = zk_inbox_client::batch_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID, 0);
        chain.set_account(batch0, open_not_finalized_bytes(5, 2));

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            1,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(1));
    }

    /// The prev batch was `AbandonBatch`ed (account gone) — nothing to compare against; proceed normally.
    #[tokio::test]
    async fn a_missing_prev_batch_proceeds_normally() {
        let chain = FakeChain::default();
        let (cursor_pda, _) = zk_inbox_client::cursor_pda(&PROGRAM, &SETTLEMENT_PROGRAM, CHAIN_ID);
        chain.set_account(cursor_pda, cursor_bytes(CHAIN_ID, 1));
        // batch 0's account is deliberately absent (abandoned).

        let outcome = resolve_batch_id(
            &chain,
            &PROGRAM,
            &SETTLEMENT_PROGRAM,
            CHAIN_ID,
            b"x",
            3200,
            1,
        )
        .await
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::PostUnder(1));
    }

    /// The short-page guard lived only inside the `RpcClient` impl; the paged probe itself
    /// zipped ids with whatever length came back, silently dropping the tail ids of a short page — exactly
    /// the ids a startup recovery exists to find. The probe now refuses a short page by name whatever the
    /// backend.
    struct ShortPageAccounts;
    impl AccountOps for ShortPageAccounts {
        async fn get_account(&self, _pubkey: &Pubkey) -> Result<Option<Vec<u8>>, ResolveError> {
            Ok(None)
        }
        async fn accounts_exist(&self, pubkeys: &[Pubkey]) -> Result<Vec<bool>, ResolveError> {
            Ok(vec![false; pubkeys.len()])
        }
        async fn get_multiple_account_data(
            &self,
            pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<Vec<u8>>>, ResolveError> {
            Ok(vec![None; pubkeys.len().saturating_sub(1)]) // one entry short
        }
    }

    #[tokio::test]
    async fn a_short_page_from_any_backend_is_a_named_refusal_not_a_truncated_scan() {
        let ids: Vec<u64> = (0..7).collect();
        let err = probe_batch_states_paged(
            &ShortPageAccounts,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            7,
            &ids,
        )
        .await
        .expect_err("a page with fewer entries than ids asked must be refused");
        assert!(
            matches!(
                err,
                ResolveError::ShortPage {
                    expected: 7,
                    got: 6
                }
            ),
            "got: {err}"
        );
    }
}
