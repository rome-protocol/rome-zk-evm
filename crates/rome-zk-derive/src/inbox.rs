//! `InboxRetrieval`: reads a finalized batch's sealed chunks, in `idx` order, and hands
//! back each chunk's raw frame bytes (a chunk body is exactly one `Frame::to_bytes()`).
//!
//! **Standing rule for every DA reader (derive, indexer, challenger):** a chunk being `sealed` on a
//! `finalized` batch was previously trusted outright — nothing
//! recomputed the accumulator's commitment over the bytes actually read back, so a chunk account whose
//! data was corrupted (or swapped) *after* finalization would be silently accepted. [`Self::chunks`] now
//! recomputes `leaf_i = keccak(body_i)` for every chunk it reads, reduces them through the same
//! `zk_inbox_client::reference_commitment` the on-chain accumulator itself is built from (owner:
//! `rome-zk-merkle` for the Merkle reduction, `rome-zk-layouts` for the `acc` formula — never
//! reimplemented here), and compares `(root, forced_root, acc)` against what [`crate::traversal::BatchRef`]
//! already read from the batch account. A mismatch is [`PipelineError::Critical`].

use solana_program::pubkey::Pubkey;

use crate::reader::AccountReader;
use crate::traversal::BatchRef;
use crate::PipelineError;

/// Reads every chunk `0..expected_count` of one finalized batch, in `idx` order ("read
/// chunks idx 0..k for batch"), returning each chunk's frame bytes (the account's data past the
/// 64-byte chunk header — `zk_inbox::HEADER_LEN` — which is exactly one `channel::Frame::to_bytes()`
/// encoding).
pub struct InboxRetrieval<R> {
    reader: R,
    program_id: Pubkey,
}

impl<R: AccountReader> InboxRetrieval<R> {
    pub fn new(reader: R, program_id: Pubkey) -> Self {
        Self { reader, program_id }
    }

    /// The underlying [`AccountReader`] — mainly for tests to inspect a fake reader's own call log
    /// (proving [`Self::chunks`] pages its reads instead of one RPC per chunk).
    pub fn reader(&self) -> &R {
        &self.reader
    }

    /// "Only sealed chunks of finalized batches" — a chunk missing, unsealed, or
    /// mismatched (wrong `chain_id`/`batch`/`idx`) is [`PipelineError::Critical`]: [`crate::traversal::SolanaTraversal`]
    /// already confirmed the covering batch account is `finalized`, so every one of its
    /// `expected_count` chunks *must* exist and be sealed — the accumulator's own `FinalizeBatch`
    /// requires `leaves_present == expected_count` before it will set `finalized`, so a
    /// missing/unsealed chunk here can only mean this crate is reading the wrong PDA or program id, not
    /// a legitimately-incomplete batch.
    ///
    /// Every chunk's PDA is derived up front and read in one
    /// [`AccountReader::get_multiple_account_data`] call — `RpcAccountReader` pages that into
    /// `getMultipleAccounts` calls of ≤ 100 keys (`reader::MAX_ACCOUNTS_PER_GET_MULTIPLE`) instead of
    /// one `getAccountInfo` per chunk; the per-chunk header/leaf checks below are unchanged.
    pub async fn chunks(&mut self, batch: BatchRef) -> Result<Vec<Vec<u8>>, PipelineError> {
        let chunk_pdas: Vec<Pubkey> = (0..batch.expected_count)
            .map(|idx| {
                zk_inbox_client::chunk_pda(&self.program_id, batch.chain_id, batch.batch, idx).0
            })
            .collect();
        let chunk_datas = self.reader.get_multiple_account_data(&chunk_pdas).await?;

        let mut out = Vec::with_capacity(batch.expected_count as usize);
        for (idx, data) in chunk_datas.into_iter().enumerate() {
            let idx = idx as u32;
            let chunk_pda = chunk_pdas[idx as usize];
            let Some(data) = data else {
                return Err(PipelineError::Critical(format!(
                    "batch {} chunk {idx} at {chunk_pda} is missing but the batch is finalized",
                    batch.batch
                )));
            };
            // The chunk header is read through its one owner (`rome-zk-layouts::chunk::read`, the same
            // decoder the inbox program and both clients use) rather than a field-by-field re-decode here.
            let fields = rome_zk_layouts::chunk::read(&data).map_err(|e| match e {
                rome_zk_layouts::LayoutError::TooShort { got, .. } => PipelineError::Critical(
                    format!("batch {} chunk {idx}: account too short for the chunk header ({got} bytes)", batch.batch),
                ),
                rome_zk_layouts::LayoutError::BadMagic => PipelineError::Critical(format!(
                    "batch {} chunk {idx}: bad chunk magic",
                    batch.batch
                )),
                // Chunk headers never carry a version byte — `chunk::read` cannot produce this variant;
                // named rather than `unreachable!()` so a future layout change surfaces here as a normal
                // error instead of a panic.
                rome_zk_layouts::LayoutError::BadVersion => PipelineError::Critical(format!(
                    "batch {} chunk {idx}: unexpected version field in chunk header",
                    batch.batch
                )),
                // Only the public-values unpacker produces these; a chunk header cannot.
                rome_zk_layouts::LayoutError::BadZiskWord { index }
                | rome_zk_layouts::LayoutError::BadZiskTail { index } => {
                    PipelineError::Critical(format!(
                        "batch {} chunk {idx}: unexpected ZisK packing error at word {index} in chunk header",
                        batch.batch
                    ))
                }
            })?;
            if fields.chain_id != batch.chain_id || fields.batch != batch.batch || fields.idx != idx
            {
                return Err(PipelineError::Critical(format!(
                    "batch {} chunk {idx} at {chunk_pda}: header reports (chain_id={}, batch={}, idx={}), expected (chain_id={}, batch={}, idx={idx})",
                    batch.batch, fields.chain_id, fields.batch, fields.idx, batch.chain_id, batch.batch
                )));
            }
            if !fields.sealed {
                return Err(PipelineError::Critical(format!(
                    "batch {} chunk {idx} is not sealed but the batch is finalized",
                    batch.batch
                )));
            }
            let len = fields.len as usize;
            let body_start = zk_inbox::HEADER_LEN;
            let body_end = body_start
                .checked_add(len)
                .filter(|&end| end <= data.len())
                .ok_or_else(|| {
                    PipelineError::Critical(format!(
                        "batch {} chunk {idx}: declared len {len} overruns the account ({} bytes)",
                        batch.batch,
                        data.len()
                    ))
                })?;
            out.push(data[body_start..body_end].to_vec());
        }

        // Recompute the commitment over the bytes actually read, compare to the batch
        // account's own — never trust a `sealed`+`finalized` chunk body outright.
        let chunk_hashes: Vec<[u8; 32]> = out
            .iter()
            .map(|body| alloy_primitives::keccak256(body).0)
            .collect();
        let (root, forced_root, acc) = zk_inbox_client::reference_commitment(
            batch.chain_id,
            batch.batch,
            batch.open_slot,
            &chunk_hashes,
        );
        if root != batch.root || forced_root != batch.forced_root || acc != batch.acc {
            return Err(PipelineError::Critical(format!(
                "batch {}: recomputed commitment (root={root:02x?}, acc={acc:02x?}) does not match \
                 the batch account (root={:02x?}, acc={:02x?}) — chunk bytes read do not match what \
                 was finalized",
                batch.batch, batch.root, batch.acc
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::AccountReader;
    use std::collections::HashMap;

    /// A fake reader that pages at the crate's own
    /// [`crate::reader::MAX_ACCOUNTS_PER_GET_MULTIPLE`] constant — not a locally hard-coded page size —
    /// and counts single-key [`AccountReader::get_account_data`] calls too, so a revert to the old
    /// per-chunk loop is caught by a `!= 0` assertion instead of silently satisfying a `<=` bound.
    #[derive(Default)]
    struct FakeReader {
        accounts: HashMap<Pubkey, Vec<u8>>,
        /// Incremented once per [`AccountReader::get_account_data`] call — must stay 0 for every test in
        /// this file, since [`InboxRetrieval::chunks`] only ever calls the batched method.
        pub single_get_calls: usize,
        /// Incremented once per simulated `getMultipleAccounts` round trip (page).
        pub multi_get_calls: usize,
    }
    impl AccountReader for FakeReader {
        async fn get_account_data(
            &mut self,
            pubkey: Pubkey,
        ) -> Result<Option<Vec<u8>>, PipelineError> {
            self.single_get_calls += 1;
            Ok(self.accounts.get(&pubkey).cloned())
        }

        async fn get_multiple_account_data(
            &mut self,
            pubkeys: &[Pubkey],
        ) -> Result<Vec<Option<Vec<u8>>>, PipelineError> {
            let mut out = Vec::with_capacity(pubkeys.len());
            for page in pubkeys.chunks(crate::reader::MAX_ACCOUNTS_PER_GET_MULTIPLE) {
                self.multi_get_calls += 1;
                for &pk in page {
                    out.push(self.accounts.get(&pk).cloned());
                }
            }
            Ok(out)
        }
    }

    /// Builds a chunk account's bytes through the layouts crate's own writer
    /// (`rome_zk_layouts::chunk::write_header`) — the same encoder the inbox program itself uses — never
    /// a hand-poked offset, so these fixtures stay honest against the real on-disk shape.
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

    /// A [`BatchRef`] whose `root`/`forced_root`/`acc` are correctly computed over `bodies` (via the
    /// same `reference_commitment` [`InboxRetrieval::chunks`] itself uses) — the positive-case fixture.
    fn batch_ref_for(chain_id: u64, batch: u64, open_slot: u64, bodies: &[&[u8]]) -> BatchRef {
        let chunk_hashes: Vec<[u8; 32]> = bodies
            .iter()
            .map(|b| alloy_primitives::keccak256(b).0)
            .collect();
        let (root, forced_root, acc) =
            zk_inbox_client::reference_commitment(chain_id, batch, open_slot, &chunk_hashes);
        BatchRef {
            chain_id,
            batch,
            open_slot,
            open_unix_ts: 1_700_000_000,
            expected_count: bodies.len() as u32,
            root,
            forced_root,
            acc,
        }
    }

    /// A [`BatchRef`] with zeroed commitment fields — fine for tests whose expected failure (missing or
    /// unsealed chunk) is raised before the commitment is ever recomputed.
    fn batch_ref(chain_id: u64, batch: u64, expected_count: u32) -> BatchRef {
        BatchRef {
            chain_id,
            batch,
            open_slot: 1,
            open_unix_ts: 1_700_000_000,
            expected_count,
            root: [0u8; 32],
            forced_root: [0u8; 32],
            acc: [0u8; 32],
        }
    }

    #[tokio::test]
    async fn reads_every_chunk_body_in_idx_order() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        let bodies: [&[u8]; 3] = [b"chunk-0-body", b"chunk-1-body", b"chunk-2-body"];
        for (idx, body) in bodies.iter().enumerate() {
            let (pda, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, idx as u32);
            reader
                .accounts
                .insert(pda, chunk_account(7, 3, idx as u32, true, body));
        }
        let mut r = InboxRetrieval::new(reader, program_id);
        let got = r.chunks(batch_ref_for(7, 3, 1, &bodies)).await.unwrap();
        assert_eq!(
            got,
            vec![bodies[0].to_vec(), bodies[1].to_vec(), bodies[2].to_vec()]
        );
    }

    /// 250 chunks must read in exactly
    /// `ceil(250/MAX_ACCOUNTS_PER_GET_MULTIPLE)` round trips, all through the batched call, none through
    /// `get_account_data` — an EXACT assertion, not `<=`, and paged at the crate's own real constant, so
    /// this fails under EITHER of the two ways a weaker test would stay green: reverting
    /// `InboxRetrieval::chunks` to one `get_account_data` per chunk (`single_get_calls` would be 250, not
    /// 0), or shrinking `MAX_ACCOUNTS_PER_GET_MULTIPLE` to 1 (`multi_get_calls` would be 250, not 3).
    #[tokio::test]
    async fn chunks_reads_in_pages_not_one_round_trip_per_chunk() {
        let program_id = Pubkey::new_unique();
        let n = 250u32;
        let bodies: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("chunk-{i}-body").into_bytes())
            .collect();
        let body_refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
        let batch_ref = batch_ref_for(7, 3, 1, &body_refs);

        let mut reader = FakeReader::default();
        for (idx, body) in bodies.iter().enumerate() {
            let (pda, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, idx as u32);
            reader
                .accounts
                .insert(pda, chunk_account(7, 3, idx as u32, true, body));
        }

        let mut r = InboxRetrieval::new(reader, program_id);
        let got = r.chunks(batch_ref).await.unwrap();
        assert_eq!(got.len(), n as usize);
        assert_eq!(
            r.reader().single_get_calls,
            0,
            "must never fall back to one get_account_data per chunk"
        );
        // Pinned independently of the formula below (mirrors `config::DEFAULT_BLOCKS_PER_BATCH`'s own
        // pin test): a formula computed purely from `MAX_ACCOUNTS_PER_GET_MULTIPLE` would silently
        // track a shrunk constant (set it to 1, say) and this assertion would stay green even
        // though every chunk now costs its own round trip — pinning the constant's real value here
        // catches that regression in this same test, not only via a separate reader-level test.
        assert_eq!(
            crate::reader::MAX_ACCOUNTS_PER_GET_MULTIPLE,
            100,
            "this test's expected round-trip count assumes Solana's real getMultipleAccounts page size"
        );
        assert_eq!(
            r.reader().multi_get_calls,
            (n as usize).div_ceil(crate::reader::MAX_ACCOUNTS_PER_GET_MULTIPLE),
            "must page at exactly the crate's own MAX_ACCOUNTS_PER_GET_MULTIPLE"
        );
    }

    /// A chunk body that no longer matches what the batch account committed
    /// to (someone corrupted the account's data after finalization, or this reader is simply lying) must
    /// be rejected (chunk bodies must not be trusted outright once `sealed`+`finalized`).
    #[tokio::test]
    async fn a_chunk_body_not_matching_the_committed_acc_is_critical() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        let bodies: [&[u8]; 2] = [b"chunk-0-body", b"chunk-1-body"];
        // BatchRef's commitment is computed over the ORIGINAL bodies...
        let batch_ref = batch_ref_for(7, 3, 1, &bodies);
        for (idx, body) in bodies.iter().enumerate() {
            let (pda, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, idx as u32);
            reader
                .accounts
                .insert(pda, chunk_account(7, 3, idx as u32, true, body));
        }
        // ...but the account actually read back has chunk 1 tampered — one flipped byte.
        let (pda1, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, 1);
        reader
            .accounts
            .insert(pda1, chunk_account(7, 3, 1, true, b"chunk-1-BODY"));

        let mut r = InboxRetrieval::new(reader, program_id);
        let err = r.chunks(batch_ref).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[tokio::test]
    async fn a_missing_chunk_is_critical() {
        let program_id = Pubkey::new_unique();
        let reader = FakeReader::default(); // nothing seeded
        let mut r = InboxRetrieval::new(reader, program_id);
        let err = r.chunks(batch_ref(7, 3, 1)).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[tokio::test]
    async fn an_unsealed_chunk_is_critical() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        let (pda, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, 0);
        reader
            .accounts
            .insert(pda, chunk_account(7, 3, 0, false, b"x"));
        let mut r = InboxRetrieval::new(reader, program_id);
        let err = r.chunks(batch_ref(7, 3, 1)).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    #[tokio::test]
    async fn a_header_reporting_the_wrong_batch_is_critical() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeReader::default();
        let (pda, _) = zk_inbox_client::chunk_pda(&program_id, 7, 3, 0);
        // Header says batch 4, but this is stored under batch 3's PDA.
        reader
            .accounts
            .insert(pda, chunk_account(7, 4, 0, true, b"x"));
        let mut r = InboxRetrieval::new(reader, program_id);
        let err = r.chunks(batch_ref(7, 3, 1)).await.unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }
}
