//! [`AccountReader`]: the one seam [`crate::traversal::SolanaTraversal`] and [`crate::inbox::InboxRetrieval`]
//! use to read Solana accounts — a real, FINALIZED-commitment RPC client in production
//! ([`RpcAccountReader`]), and (in this crate's own tests) a thin adapter over
//! `solana-program-test`'s `BanksClient`. Read-only by construction (this node only READS
//! Solana and sends no transactions — V1-free: none of the SIMD-0385 V1 tx-format constraints apply
//! here, since nothing here ever builds or signs a Solana tx).

use solana_program::pubkey::Pubkey;

use crate::PipelineError;

/// Reads one account's raw data, or `None` if the account does not exist (yet, or ever). A future
/// derivation-node profile that reads accounts a different way (e.g. a local
/// snapshot cache, a different RPC pool) implements this trait instead of touching any stage above it.
pub trait AccountReader: Send {
    fn get_account_data(
        &mut self,
        pubkey: Pubkey,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, PipelineError>> + Send;

    /// Reads several accounts, in the order given, in as few round trips as this backend allows.
    ///
    /// [`crate::inbox::InboxRetrieval`]
    /// used to await [`Self::get_account_data`] once per chunk — at the design point (≤ 900 chunks per
    /// 10-s batch) that is ≤ 900 sequential RPC round trips per batch, 35–90 s of
    /// reads for 10 s of chain at 50–100 ms/RTT to a remote node — a follower falls behind live
    /// batches, and a from-genesis resume multiplies it by history. Solana's `getMultipleAccounts`
    /// (100 keys/call) turns that into ≤ 9 calls/batch.
    ///
    /// Default implementation: one [`Self::get_account_data`] call per key, in order — correct (if not
    /// faster) for every backend that has no batched read of its own, in particular the test fakes and
    /// `solana-program-test`'s `BanksClient`. [`RpcAccountReader`] overrides this
    /// with the real `getMultipleAccounts`, paged.
    fn get_multiple_account_data(
        &mut self,
        pubkeys: &[Pubkey],
    ) -> impl std::future::Future<Output = Result<Vec<Option<Vec<u8>>>, PipelineError>> + Send {
        async move {
            let mut out = Vec::with_capacity(pubkeys.len());
            for &pubkey in pubkeys {
                out.push(self.get_account_data(pubkey).await?);
            }
            Ok(out)
        }
    }
}

/// `getMultipleAccounts`' own per-call key limit (Solana RPC; also `RpcAccountReader`'s page size).
pub const MAX_ACCOUNTS_PER_GET_MULTIPLE: usize = 100;

/// Production implementation: a real Solana RPC node, always read at
/// [`solana_commitment_config::CommitmentConfig::finalized`] (chunks are read at
/// Solana FINALIZED commitment) — the one commitment level under which a chunk or batch account this
/// node has seen can never be rolled back out from under it.
pub struct RpcAccountReader {
    client: solana_client::nonblocking::rpc_client::RpcClient,
}

impl RpcAccountReader {
    pub fn new(url: String) -> Self {
        Self {
            client: solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                url,
                solana_commitment_config::CommitmentConfig::finalized(),
            ),
        }
    }
}

impl AccountReader for RpcAccountReader {
    async fn get_account_data(&mut self, pubkey: Pubkey) -> Result<Option<Vec<u8>>, PipelineError> {
        match self
            .client
            .get_account_with_commitment(
                &pubkey,
                solana_commitment_config::CommitmentConfig::finalized(),
            )
            .await
        {
            Ok(resp) => Ok(resp.value.map(|a| a.data)),
            // Solana's own RPC returns success with `value: null` for a missing account (handled
            // above) — an `Err` here is a genuine RPC-layer problem (timeout, node lagging, rate
            // limit), never "account doesn't exist yet"; that is always Temporary, never Critical —
            // this node must keep polling, not treat a transient RPC hiccup as a strict-validity fault.
            Err(e) => Err(PipelineError::Temporary(format!(
                "getAccountInfo({pubkey}): {e}"
            ))),
        }
    }

    /// Real `getMultipleAccounts`, paged at [`MAX_ACCOUNTS_PER_GET_MULTIPLE`] keys
    /// per call, always at FINALIZED commitment — the same guarantee [`Self::get_account_data`] gives.
    /// Solana's own RPC preserves key order in the response, so pages concatenate directly into the
    /// caller's original order.
    ///
    /// A page that comes back short (a provider truncating below
    /// `MAX_ACCOUNTS_PER_GET_MULTIPLE`, e.g. a rate limit) is [`PipelineError::Temporary`], not silently
    /// concatenated — an unchecked short page shifts every later chunk in the caller's index space, which
    /// [`crate::inbox::InboxRetrieval`] would otherwise surface as a strict-validity `Critical` (a header
    /// idx mismatch) instead of the ordinary transport anomaly it actually is.
    async fn get_multiple_account_data(
        &mut self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, PipelineError> {
        let mut out = Vec::with_capacity(pubkeys.len());
        for page in pubkeys.chunks(MAX_ACCOUNTS_PER_GET_MULTIPLE) {
            let resp = self
                .client
                .get_multiple_accounts_with_commitment(
                    page,
                    solana_commitment_config::CommitmentConfig::finalized(),
                )
                .await
                .map_err(|e| PipelineError::Temporary(format!("getMultipleAccounts: {e}")))?;
            if resp.value.len() != page.len() {
                return Err(PipelineError::Temporary(format!(
                    "getMultipleAccounts: {} of {} entries",
                    resp.value.len(),
                    page.len()
                )));
            }
            out.extend(resp.value.into_iter().map(|a| a.map(|a| a.data)));
        }
        Ok(out)
    }
}
