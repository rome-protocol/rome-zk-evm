//! The Solana side: read a finalized batch's account + chunk bodies exactly the way
//! `rome-zk-derive::inbox::InboxRetrieval` does (the same
//! `zk_inbox_client`/`rome_zk_layouts` functions, never a field-by-field re-decode), recompute `acc` and
//! assert it against the batch account's own, then decode the channel to learn the batch's own
//! `first..=last` block range before any reth-verifier RPC is made.
//!
//! Self-contained (no dependency on `rome-zk-derive` itself, which pulls sqlx/Postgres for its own
//! pipeline state) — a minimal [`AccountFetch`] trait stands in for `rome-zk-derive::reader::AccountReader`
//! so this crate's tests use a fake account map instead of a live RPC.

use solana_program::pubkey::Pubkey;

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("batch account decode failed: {0}")]
    BatchDecode(String),
    /// The batch's own `finalized` flag is `false` — this tool builds a guest input from a batch's chunk
    /// bodies, which only settle to a stable, provable shape once the batch is finalized;
    /// an open batch's DA can still change under it.
    #[error("batch {batch} (chain {chain_id}) is not finalized yet")]
    BatchNotFinalized { chain_id: u64, batch: u64 },
    #[error("chunk {idx} at {pda} is missing")]
    ChunkMissing { idx: u32, pda: Pubkey },
    #[error("chunk {idx}: header decode failed: {reason}")]
    ChunkHeaderDecode { idx: u32, reason: String },
    #[error("chunk {idx}: header reports (chain_id={got_chain_id}, batch={got_batch}, idx={got_idx}), expected (chain_id={expected_chain_id}, batch={expected_batch}, idx={idx})")]
    ChunkHeaderMismatch {
        idx: u32,
        got_chain_id: u64,
        got_batch: u64,
        got_idx: u32,
        expected_chain_id: u64,
        expected_batch: u64,
    },
    #[error("chunk {idx} is not sealed")]
    ChunkNotSealed { idx: u32 },
    #[error("chunk {idx}: declared len {len} overruns the account ({account_len} bytes)")]
    ChunkBodyOverrun {
        idx: u32,
        len: u32,
        account_len: usize,
    },
    #[error("acc mismatch: recomputed acc {recomputed} does not match the batch account's own {on_chain}")]
    AccMismatch {
        recomputed: String,
        on_chain: String,
    },
    #[error("channel decode failed: {0}")]
    ChannelDecode(String),
    /// An [`AccountFetch`] call itself failed — transport-level, never
    /// a decoded on-chain fact (that stays `Ok(None)` for a genuinely missing account, see
    /// [`AccountFetch`]'s own doc). Callers retry the same batch on this rather than halting.
    #[error("account fetch: {0}")]
    Fetch(#[from] FetchError),
}

/// An [`AccountFetch`] call itself failed — transport-level (a dropped connection, a rate limit, a
/// timed-out RPC call), never a panic and never confused with a genuinely missing account (which stays
/// `Ok(None)`). Carries a plain description rather than a specific transport error type so this trait
/// stays independent of which Solana client generation a real implementation uses — mirrors
/// `rome_zk_prover::anchor::FetchError` exactly (this crate is standalone, module doc: no dependency on
/// that crate), a deliberate, tested duplication of the same small shape.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// `getMultipleAccounts`' own per-call key limit (Solana RPC) — the page size a real
/// [`AccountFetch::get_multiple_accounts`] implementation pages at (`rome-zk-derive`'s own
/// `reader::MAX_ACCOUNTS_PER_GET_MULTIPLE`, mirrored here rather than depending on that crate — this
/// crate is self-contained, module doc above).
pub const MAX_ACCOUNTS_PER_GET_MULTIPLE: usize = 100;

/// Stands in for a Solana RPC connection — one account per call ([`Self::get_account`]), or several in
/// as few round trips as the backend allows ([`Self::get_multiple_accounts`]). Implemented for a live RPC
/// client by the CLI binary; implemented for a `HashMap` fixture by this module's own tests.
pub trait AccountFetch {
    /// `Ok(None)` for a genuinely missing/uninitialized account — a decoded on-chain fact.
    /// `Err(FetchError)` for the READ ITSELF failing (a dropped
    /// connection, a rate limit, a timeout) — never a panic, and never conflated with the account
    /// simply not existing: a caller that cannot tell these apart cannot safely retry only the transient
    /// case.
    fn get_account(&mut self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, FetchError>;

    /// Reads several accounts, in the order given. [`fetch_and_verify_batch`] used to await
    /// [`Self::get_account`] once per chunk — at the design point (≤ 900 chunks/batch)
    /// that is ≤ 900 sequential RPC round trips per batch. `getMultipleAccounts` (100 keys/call) turns
    /// that into ≤ 9 calls/batch.
    ///
    /// Default implementation: one [`Self::get_account`] call per key, in order, failing the WHOLE call
    /// on the first error (correct, if not faster, for any backend with no batched read of its own — this
    /// module's simple `HashMap` fixture tests use it as-is via [`Self::get_account`]). The CLI's real
    /// RPC-backed implementation overrides this with the real `getMultipleAccounts`, paged at
    /// [`MAX_ACCOUNTS_PER_GET_MULTIPLE`] keys per call.
    fn get_multiple_accounts(
        &mut self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, FetchError> {
        pubkeys.iter().map(|k| self.get_account(k)).collect()
    }
}

/// The batch account's decoded fields this crate needs, re-exported from `zk_inbox_client::BatchAccount`
/// so callers do not need that crate's own import path.
pub type BatchAccount = zk_inbox_client::BatchAccount;

/// Reads the batch account (`batch_pda`) and every one of its `expected_count` chunk bodies (`chunk_pda`,
/// `idx` order), recomputes `(root, forced_root, acc)` over the bytes actually read, and asserts it
/// against the batch account's own recorded values — the same defense-in-depth check
/// `rome-zk-derive::inbox::InboxRetrieval::chunks` makes (a `sealed`+`finalized` chunk is never
/// trusted outright). Returns `(batch_account, chunk_bodies)` in `idx` order on success.
pub fn fetch_and_verify_batch(
    fetch: &mut impl AccountFetch,
    inbox_program_id: &Pubkey,
    settlement_program: &Pubkey,
    chain_id: u64,
    batch: u64,
) -> Result<(BatchAccount, Vec<Vec<u8>>), InboxError> {
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(inbox_program_id, settlement_program, chain_id, batch);
    let batch_data = fetch
        .get_account(&batch_pda)?
        .ok_or_else(|| InboxError::BatchDecode(format!("batch account {batch_pda} not found")))?;
    let batch_account = zk_inbox_client::decode_batch_account(&batch_data)
        .map_err(|e| InboxError::BatchDecode(e.to_string()))?;
    if !batch_account.finalized {
        return Err(InboxError::BatchNotFinalized { chain_id, batch });
    }

    let chunk_pdas: Vec<Pubkey> = (0..batch_account.expected_count)
        .map(|idx| {
            zk_inbox_client::chunk_pda(inbox_program_id, settlement_program, chain_id, batch, idx).0
        })
        .collect();
    let chunk_datas = fetch.get_multiple_accounts(&chunk_pdas)?;

    let mut chunk_bodies = Vec::with_capacity(batch_account.expected_count as usize);
    for (idx, data) in chunk_datas.into_iter().enumerate() {
        let idx = idx as u32;
        let chunk_pda = chunk_pdas[idx as usize];
        let data = data.ok_or(InboxError::ChunkMissing {
            idx,
            pda: chunk_pda,
        })?;
        let fields =
            rome_zk_layouts::chunk::read(&data).map_err(|e| InboxError::ChunkHeaderDecode {
                idx,
                reason: format!("{e:?}"),
            })?;
        if fields.chain_id != chain_id || fields.batch != batch || fields.idx != idx {
            return Err(InboxError::ChunkHeaderMismatch {
                idx,
                got_chain_id: fields.chain_id,
                got_batch: fields.batch,
                got_idx: fields.idx,
                expected_chain_id: chain_id,
                expected_batch: batch,
            });
        }
        if !fields.sealed {
            return Err(InboxError::ChunkNotSealed { idx });
        }
        let len = fields.len as usize;
        let body_start = rome_zk_layouts::chunk::HEADER_LEN;
        let body_end = body_start
            .checked_add(len)
            .filter(|&end| end <= data.len())
            .ok_or(InboxError::ChunkBodyOverrun {
                idx,
                len: fields.len,
                account_len: data.len(),
            })?;
        chunk_bodies.push(data[body_start..body_end].to_vec());
    }

    let chunk_hashes: Vec<[u8; 32]> = chunk_bodies
        .iter()
        .map(|body| rome_zk_merkle::keccak256(&[body]))
        .collect();
    let (root, forced_root, acc) = zk_inbox_client::reference_commitment(
        chain_id,
        batch,
        batch_account.open_slot,
        &chunk_hashes,
    );
    if root != batch_account.root
        || forced_root != batch_account.forced_root
        || acc != batch_account.acc
    {
        return Err(InboxError::AccMismatch {
            recomputed: hex::encode(acc),
            on_chain: hex::encode(batch_account.acc),
        });
    }

    Ok((batch_account, chunk_bodies))
}

/// Decodes the channel (the same pure-Rust path the guest itself uses, `rome_zk_channel::decode_stream`
/// — the C codec is fine here, this is a host tool) to learn the batch's own `first..=last` block numbers
/// before any reth-verifier RPC is made.
pub fn decode_block_range(chunk_bodies: &[Vec<u8>]) -> Result<(u64, u64), InboxError> {
    let frames: Vec<rome_zk_channel::Frame> = chunk_bodies
        .iter()
        .map(|b| rome_zk_channel::Frame::from_bytes(b))
        .collect::<Result<_, _>>()
        .map_err(|e| InboxError::ChannelDecode(format!("{e:?}")))?;
    let compressed = rome_zk_channel::reassemble(&frames)
        .map_err(|e| InboxError::ChannelDecode(format!("{e:?}")))?;
    let blocks = rome_zk_channel::decode_stream(&compressed)
        .map_err(|e| InboxError::ChannelDecode(format!("{e:?}")))?;
    let first = blocks
        .first()
        .ok_or_else(|| InboxError::ChannelDecode("empty block range".into()))?
        .number;
    let last = blocks.last().unwrap().number;
    Ok((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeFetch {
        accounts: HashMap<Pubkey, Vec<u8>>,
    }
    impl AccountFetch for FakeFetch {
        fn get_account(&mut self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, FetchError> {
            Ok(self.accounts.get(pubkey).cloned())
        }
    }

    fn chunk_account(chain_id: u64, batch: u64, idx: u32, sealed: bool, body: &[u8]) -> Vec<u8> {
        let header =
            rome_zk_layouts::chunk::write_header(&rome_zk_layouts::chunk::ChunkHeaderFields {
                authority: [0u8; 32],
                chain_id,
                batch,
                idx,
                len: body.len() as u32,
                sealed,
            });
        let mut d = header.to_vec();
        d.extend_from_slice(body);
        d
    }

    /// Load-bearing: builds a batch account + one sealed chunk with the real
    /// layout writers, an `acc` that matches, and asserts `fetch_and_verify_batch` recovers the chunk body
    /// byte-for-byte.
    #[test]
    fn fetch_and_verify_batch_reads_a_real_shaped_batch() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (chain_id, batch, open_slot) = (200101u64, 7u64, 42u64);
        let body = b"a chunk body".to_vec();
        let chunk_hash = rome_zk_merkle::keccak256(&[&body]);
        let (root, forced_root, acc) =
            zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &[chunk_hash]);

        let batch_header = rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot,
            expected_count: 1,
            leaves_present: 1,
            finalized: true,
            settlement_program: settlement_program.to_bytes(),
            authority: [0u8; 32],
            root,
            forced_root,
            acc,
            finalize_cursor: 1,
            open_unix_ts: 1_789_337_436,
        };
        let batch_data = rome_zk_layouts::batch::write_header(&batch_header);

        let mut fetch = FakeFetch::default();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&inbox_program, &settlement_program, chain_id, batch);
        fetch.accounts.insert(batch_pda, batch_data.to_vec());
        let (chunk_pda, _) =
            zk_inbox_client::chunk_pda(&inbox_program, &settlement_program, chain_id, batch, 0);
        fetch
            .accounts
            .insert(chunk_pda, chunk_account(chain_id, batch, 0, true, &body));

        let (account, bodies) = fetch_and_verify_batch(
            &mut fetch,
            &inbox_program,
            &settlement_program,
            chain_id,
            batch,
        )
        .unwrap();
        assert_eq!(account.acc, acc);
        assert_eq!(account.open_unix_ts, 1_789_337_436);
        assert_eq!(bodies, vec![body]);
    }

    /// Mutation: a batch account whose own `finalized` flag is `false` is refused by name
    /// (`BatchNotFinalized`), before any chunk is even read — an open batch's DA can still change
    /// under it, so this tool must never build a guest input from one.
    #[test]
    fn fetch_and_verify_batch_refuses_an_unfinalized_batch() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (chain_id, batch, open_slot) = (200101u64, 7u64, 42u64);
        let body = b"a chunk body".to_vec();
        let chunk_hash = rome_zk_merkle::keccak256(&[&body]);
        let (root, forced_root, acc) =
            zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &[chunk_hash]);

        let batch_header = rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot,
            expected_count: 1,
            leaves_present: 1,
            finalized: false, // still open — not finalized
            settlement_program: settlement_program.to_bytes(),
            authority: [0u8; 32],
            root,
            forced_root,
            acc,
            finalize_cursor: 1,
            open_unix_ts: 1_789_337_436,
        };
        let batch_data = rome_zk_layouts::batch::write_header(&batch_header);

        let mut fetch = FakeFetch::default();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&inbox_program, &settlement_program, chain_id, batch);
        fetch.accounts.insert(batch_pda, batch_data.to_vec());
        // Deliberately no chunk account inserted — a BatchNotFinalized refusal must fire before any
        // chunk is looked up at all.

        let err = fetch_and_verify_batch(
            &mut fetch,
            &inbox_program,
            &settlement_program,
            chain_id,
            batch,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                InboxError::BatchNotFinalized {
                    chain_id: got_chain_id,
                    batch: got_batch,
                } if got_chain_id == chain_id && got_batch == batch
            ),
            "got {err:?}"
        );
    }

    /// A fetch that always errors.
    struct AlwaysErrorsFetch;
    impl AccountFetch for AlwaysErrorsFetch {
        fn get_account(&mut self, _pubkey: &Pubkey) -> Result<Option<Vec<u8>>, FetchError> {
            Err(FetchError("simulated transient RPC error".to_string()))
        }
    }

    /// A transport-level failure of the batch account read itself is a distinct,
    /// named `InboxError::Fetch` — never conflated with the account genuinely being missing
    /// (`BatchDecode`), and never a panic.
    #[test]
    fn a_fetch_error_on_the_batch_account_read_is_a_named_fetch_error_not_a_missing_batch() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let err = fetch_and_verify_batch(
            &mut AlwaysErrorsFetch,
            &inbox_program,
            &settlement_program,
            200101,
            7,
        )
        .unwrap_err();
        assert!(
            matches!(err, InboxError::Fetch(_)),
            "a transport failure must be InboxError::Fetch, not BatchDecode; got {err:?}"
        );
    }

    /// Mutation: a chunk body that does not match the batch account's recorded `acc` is refused by name
    /// (`AccMismatch`), not silently accepted.
    #[test]
    fn fetch_and_verify_batch_refuses_a_tampered_chunk_body() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (chain_id, batch, open_slot) = (200101u64, 7u64, 42u64);
        let real_body = b"the real body".to_vec();
        let chunk_hash = rome_zk_merkle::keccak256(&[&real_body]);
        let (root, forced_root, acc) =
            zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &[chunk_hash]);
        let batch_header = rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot,
            expected_count: 1,
            leaves_present: 1,
            finalized: true,
            settlement_program: settlement_program.to_bytes(),
            authority: [0u8; 32],
            root,
            forced_root,
            acc,
            finalize_cursor: 1,
            open_unix_ts: 1,
        };
        let batch_data = rome_zk_layouts::batch::write_header(&batch_header);

        let mut fetch = FakeFetch::default();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&inbox_program, &settlement_program, chain_id, batch);
        fetch.accounts.insert(batch_pda, batch_data.to_vec());
        let (chunk_pda, _) =
            zk_inbox_client::chunk_pda(&inbox_program, &settlement_program, chain_id, batch, 0);
        // Tampered body served back — same length, different bytes.
        let tampered = b"a fake replaced body!!".to_vec();
        fetch.accounts.insert(
            chunk_pda,
            chunk_account(chain_id, batch, 0, true, &tampered),
        );

        let err = fetch_and_verify_batch(
            &mut fetch,
            &inbox_program,
            &settlement_program,
            chain_id,
            batch,
        )
        .unwrap_err();
        assert!(matches!(err, InboxError::AccMismatch { .. }), "got {err:?}");
    }

    /// A fake fetch that pages `get_multiple_accounts` itself (mirroring
    /// `RpcAccountReader`'s real `getMultipleAccounts` chunking, `rome-zk-derive/src/reader.rs`) and
    /// counts both how many calls it received and each call's own key-slice length — so a mutation that
    /// widens the page size is caught even when the call COUNT alone would not change.
    #[derive(Default)]
    struct CountingPagedFetch {
        accounts: HashMap<Pubkey, Vec<u8>>,
        get_account_calls: u32,
        get_multiple_accounts_calls: u32,
        page_lens: Vec<usize>,
    }
    impl AccountFetch for CountingPagedFetch {
        fn get_account(&mut self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, FetchError> {
            self.get_account_calls += 1;
            Ok(self.accounts.get(pubkey).cloned())
        }
        fn get_multiple_accounts(
            &mut self,
            pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<Vec<u8>>>, FetchError> {
            let mut out = Vec::with_capacity(pubkeys.len());
            for page in pubkeys.chunks(MAX_ACCOUNTS_PER_GET_MULTIPLE) {
                self.get_multiple_accounts_calls += 1;
                self.page_lens.push(page.len());
                out.extend(page.iter().map(|k| self.accounts.get(k).cloned()));
            }
            Ok(out)
        }
    }

    /// A 250-chunk finalized batch, built with the real layout writers throughout (batch header +
    /// every chunk header/body) — `fetch_and_verify_batch` must read every chunk through
    /// `get_multiple_accounts`, paged at 100 keys/call (3 calls: 100 + 100 + 50), and never fall back to
    /// `get_account` at all. **RED before `get_multiple_accounts` existed**: `fetch_and_verify_batch`
    /// called `fetch.get_account` once per chunk, so this test's own `get_multiple_accounts_calls == 3`
    /// / `get_account_calls == 0` assertions failed (250 calls to `get_account`, 0 to
    /// `get_multiple_accounts`) before the change to batched reads.
    ///
    /// **Mutation: page size 101 → red.** The exact per-call page lengths are asserted
    /// (`[100, 100, 50]`), not merely the call count (`ceil(250/101)` is still 3) — so widening the page
    /// size still fails this test.
    #[test]
    fn fetch_and_verify_batch_pages_every_chunk_read_at_100_keys_per_call() {
        let inbox_program = Pubkey::new_unique();
        let settlement_program = Pubkey::new_unique();
        let (chain_id, batch, open_slot) = (200101u64, 11u64, 99u64);
        let n = 250u32;
        let bodies: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("chunk body {i}").into_bytes())
            .collect();
        let chunk_hashes: Vec<[u8; 32]> = bodies
            .iter()
            .map(|b| rome_zk_merkle::keccak256(&[b]))
            .collect();
        let (root, forced_root, acc) =
            zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &chunk_hashes);

        let batch_header = rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot,
            expected_count: n,
            leaves_present: n,
            finalized: true,
            settlement_program: settlement_program.to_bytes(),
            authority: [0u8; 32],
            root,
            forced_root,
            acc,
            finalize_cursor: n,
            open_unix_ts: 1_789_337_436,
        };
        let batch_data = rome_zk_layouts::batch::write_header(&batch_header);

        let mut fetch = CountingPagedFetch::default();
        let (batch_pda, _) =
            zk_inbox_client::batch_pda(&inbox_program, &settlement_program, chain_id, batch);
        fetch.accounts.insert(batch_pda, batch_data.to_vec());
        for (idx, body) in bodies.iter().enumerate() {
            let (chunk_pda, _) = zk_inbox_client::chunk_pda(
                &inbox_program,
                &settlement_program,
                chain_id,
                batch,
                idx as u32,
            );
            fetch.accounts.insert(
                chunk_pda,
                chunk_account(chain_id, batch, idx as u32, true, body),
            );
        }

        let (account, got_bodies) = fetch_and_verify_batch(
            &mut fetch,
            &inbox_program,
            &settlement_program,
            chain_id,
            batch,
        )
        .unwrap();
        assert_eq!(account.acc, acc);
        assert_eq!(got_bodies, bodies);
        assert_eq!(
            fetch.get_multiple_accounts_calls, 3,
            "250 chunks at 100/call must be exactly 3 get_multiple_accounts calls"
        );
        assert_eq!(
            fetch.page_lens,
            vec![100, 100, 50],
            "page sizes must be exactly [100, 100, 50] — a widened page size changes this even though \
             the call count can stay 3"
        );
        assert_eq!(
            fetch.get_account_calls, 1,
            "exactly one get_account call (the batch account itself) — every CHUNK must be read \
             through get_multiple_accounts, never a per-chunk get_account"
        );
    }
}
