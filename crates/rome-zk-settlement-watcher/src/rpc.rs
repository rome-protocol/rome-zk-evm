//! The one seam between this crate's decode/ingest logic and an actual Solana RPC endpoint —
//! [`Source`]. Production reads it via [`RpcSource`] (real `getSignaturesForAddress`/`getTransaction`,
//! per-request timeout, two-endpoint failover); `tests/support` replays it from the recorded Tiber
//! devnet fixture. Mirrors the seam shape `rome-zk-derive::reader::AccountReader` already uses in this
//! workspace — a plain trait with `impl Future<...> + Send` methods, no
//! `async_trait` dependency.

use solana_commitment_config::CommitmentConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status_client_types::{
    EncodedTransaction, TransactionConfirmationStatus, UiMessage, UiRawMessage,
    UiTransactionEncoding,
};
use std::{future::Future, str::FromStr, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("bad signature: {0}")]
    BadSignature(String),
}

/// One entry of `getSignaturesForAddress` — newest-first from a real node, oldest-first once
/// [`ingest::run_once`] normalizes a page.
#[derive(Debug, Clone)]
pub struct SignatureInfo {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    pub err: bool,
    /// `None` is treated as `Confirmed` — `getSignaturesForAddress` never returns a signature below
    /// that commitment, so a source that omits this field (a hand-built fixture, say) is exactly as
    /// provisional as the least-confirmed thing the real RPC method could ever hand back.
    pub confirmation_status: Option<TransactionConfirmationStatus>,
}

/// A decoded transaction's raw (unparsed) message — exactly the shape a real `getTransaction` call
/// with `encoding: Json` returns for a top-level instruction list. No `meta` (this crate never reads
/// chunk body bytes or logs — see `decode.rs`).
#[derive(Debug, Clone)]
pub struct RawTx {
    pub message: UiRawMessage,
    pub err: bool,
}

/// The seam: signature history + transaction bodies for one program, at whatever commitment
/// `getSignaturesForAddress` itself reports (the FINALIZED-only discipline governs
/// `rome-zk-derive`'s DA reads, not this feed — this crate's own finality pass, `finality.rs`, is
/// what turns "confirmed" into "finalized" here).
pub trait Source: Send {
    /// Pages backward from `before` (exclusive; `None` = the most recent signature), stopping at (and
    /// excluding) `until` if given — the same `before`/`until` contract `getSignaturesForAddress` has.
    fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<SignatureInfo>, SourceError>> + Send;

    /// `None` if the node has no record of this signature (never observed, or pruned) — `ingest.rs`
    /// treats that as "skip this one, note it, move on", never a hard failure (a signature
    /// `getSignaturesForAddress` just returned should always resolve, but a rare provider
    /// inconsistency must not wedge the whole page the way an RPC call with no timeout would).
    fn get_transaction(
        &mut self,
        signature: &str,
    ) -> impl Future<Output = Result<Option<RawTx>, SourceError>> + Send;

    /// Batched `getSignatureStatuses` for the finality pass (`finality.rs`) — up to 256 signatures
    /// per call (Solana's own limit); returns `None` per-signature for one the node no longer has a
    /// status for (should not happen for anything already `confirmed`, but is not this crate's place to
    /// assert that against a fixture or a lagging provider).
    fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> impl Future<Output = Result<Vec<Option<TransactionConfirmationStatus>>, SourceError>> + Send;

    /// The node's current slot at `finalized` commitment — `finality.rs`'s own reference point for the
    /// 150-slot horizon a `None`-status signature must be behind before it is called `dropped`.
    /// Never used for anything wire-format-sensitive; purely a liveness/staleness measure.
    fn current_slot(&mut self) -> impl Future<Output = Result<u64, SourceError>> + Send;
}

/// Per-HTTP-request timeout for the underlying RPC client — the same reasoning
/// `rome-zk-batcher::sender::RPC_REQUEST_TIMEOUT` documents: without an explicit ceiling, a single
/// slow or silently-dropped connection to a degraded RPC endpoint blocks that call indefinitely, and no
/// amount of this module's own failover logic can recover from a `.await` that never returns control to
/// check anything. `RpcSource::new` also wraps every call in a `tokio::time::timeout` of the same
/// duration as a second, independent guard (belt-and-braces against a timeout that a client's own HTTP
/// stack fails to enforce).
pub const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Real Solana RPC, with failover across up to two configured endpoints (a call with no
/// per-request timeout can wedge the whole page under a degraded RPC endpoint) and a per-request
/// timeout on each. Every read is at `confirmed` commitment for signatures (the least commitment
/// `getSignaturesForAddress` can report) and `confirmed` for transaction bodies — `finality.rs` is what
/// upgrades a row to `finalized` once Solana itself agrees.
pub struct RpcSource {
    endpoints: Vec<solana_client::nonblocking::rpc_client::RpcClient>,
}

impl RpcSource {
    /// `endpoint_urls` in priority order — the first that answers within
    /// [`RPC_REQUEST_TIMEOUT`] wins; a later one is tried only if an earlier one times out or errors.
    /// Panics if `endpoint_urls` is empty (a watcher with no RPC endpoint is a config error, not a
    /// runtime one).
    pub fn new(endpoint_urls: Vec<String>) -> Self {
        assert!(
            !endpoint_urls.is_empty(),
            "RpcSource needs at least one endpoint"
        );
        let endpoints = endpoint_urls
            .into_iter()
            .map(|url| {
                solana_client::nonblocking::rpc_client::RpcClient::new_with_timeout_and_commitment(
                    url,
                    RPC_REQUEST_TIMEOUT,
                    CommitmentConfig::confirmed(),
                )
            })
            .collect();
        Self { endpoints }
    }
}

fn parse_sig(s: &str) -> Result<Signature, SourceError> {
    Signature::from_str(s).map_err(|e| SourceError::BadSignature(format!("{s}: {e}")))
}

/// One endpoint's own `getTransaction` attempt -- what [`failover_get_transaction`]'s merge loop iterates
/// over. `solana_client::nonblocking::rpc_client::RpcClient` is the production implementation (the real
/// network call, wrapped in the same per-request timeout every other method here uses); `#[cfg(test)]`
/// fakes this trait directly so the merge/failover *decision* is unit-tested against
/// canned per-endpoint outcomes, not a live RPC endpoint.
trait TxAttempt: Sync {
    fn attempt(
        &self,
        sig: &Signature,
    ) -> impl Future<Output = Result<Option<RawTx>, SourceError>> + Send;
}

impl TxAttempt for solana_client::nonblocking::rpc_client::RpcClient {
    async fn attempt(&self, sig: &Signature) -> Result<Option<RawTx>, SourceError> {
        let config = solana_client::rpc_config::RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            // SIMD-0385 V1 transactions carry version byte 0x81 -- version 1, not
            // the v0 (0x80) most RPC clients default to.
            max_supported_transaction_version: Some(1),
        };
        // A raw `send::<Option<_>>` rather than `get_transaction_with_config` (which deserializes
        // straight into the non-`Option` struct): `getTransaction` returns a bare JSON `null` for a
        // signature the node has no record of (pruned, or never seen), and only `Option<T>` lets serde
        // accept that -- the typed method would surface it as a deserialize error indistinguishable
        // from a genuinely malformed response.
        let params = serde_json::json!([sig.to_string(), config]);
        match tokio::time::timeout(
            RPC_REQUEST_TIMEOUT,
            self.send::<Option<
                solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta,
            >>(
                solana_client::rpc_request::RpcRequest::GetTransaction,
                params,
            ),
        )
        .await
        {
            Ok(Ok(None)) => Ok(None),
            Ok(Ok(Some(resp))) => {
                let EncodedTransaction::Json(ui_tx) = resp.transaction.transaction else {
                    return Ok(None);
                };
                let UiMessage::Raw(message) = ui_tx.message else {
                    return Ok(None);
                };
                let err = resp
                    .transaction
                    .meta
                    .as_ref()
                    .map(|m| m.err.is_some())
                    .unwrap_or(false);
                Ok(Some(RawTx { message, err }))
            }
            Ok(Err(e)) => Err(SourceError::Rpc(e.to_string())),
            Err(_) => Err(SourceError::Rpc("getTransaction timed out".to_string())),
        }
    }
}

/// Merges every configured endpoint's own `getTransaction` attempt, in priority order, into the single
/// verdict [`RpcSource::get_transaction`] returns: the
/// first endpoint to report a real body wins immediately (later endpoints are never even asked, same as
/// before this fix). A null body from one endpoint no longer short-circuits the whole call -- Agave maps
/// a transient Bigtable read error to a bare null, so one endpoint's null means only
/// that ONE endpoint's lookup came back empty, not that the signature does not exist; the next endpoint
/// is tried. `Ok(None)` is returned only once *every* configured endpoint has reported null -- if any
/// endpoint instead errored (timed out, or a real RPC error) without a later endpoint finding the body,
/// the merged verdict is that endpoint's error, never a silent `None` papering over an inconclusive
/// answer.
async fn failover_get_transaction<E: TxAttempt>(
    endpoints: &[E],
    sig: &Signature,
) -> Result<Option<RawTx>, SourceError> {
    let mut null_count = 0usize;
    let mut last_err = None;
    for endpoint in endpoints {
        match endpoint.attempt(sig).await {
            Ok(Some(tx)) => return Ok(Some(tx)),
            Ok(None) => null_count += 1,
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    if null_count == endpoints.len() {
        return Ok(None);
    }
    Err(SourceError::Rpc(last_err.unwrap_or_default()))
}

impl Source for RpcSource {
    async fn get_signatures_for_address(
        &mut self,
        program_id: &Pubkey,
        before: Option<String>,
        until: Option<String>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, SourceError> {
        let before_sig = before.as_deref().map(parse_sig).transpose()?;
        let until_sig = until.as_deref().map(parse_sig).transpose()?;
        let mut last_err = None;
        for client in &self.endpoints {
            let config = solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                before: before_sig,
                until: until_sig,
                limit: Some(limit),
                commitment: Some(CommitmentConfig::confirmed()),
            };
            match tokio::time::timeout(
                RPC_REQUEST_TIMEOUT,
                client.get_signatures_for_address_with_config(program_id, config),
            )
            .await
            {
                Ok(Ok(page)) => {
                    return Ok(page
                        .into_iter()
                        .map(|r| SignatureInfo {
                            signature: r.signature,
                            slot: r.slot,
                            block_time: r.block_time,
                            err: r.err.is_some(),
                            confirmation_status: r.confirmation_status,
                        })
                        .collect());
                }
                Ok(Err(e)) => last_err = Some(e.to_string()),
                Err(_) => last_err = Some("getSignaturesForAddress timed out".to_string()),
            }
        }
        Err(SourceError::Rpc(last_err.unwrap_or_default()))
    }

    async fn get_transaction(&mut self, signature: &str) -> Result<Option<RawTx>, SourceError> {
        let sig = parse_sig(signature)?;
        failover_get_transaction(&self.endpoints, &sig).await
    }

    async fn get_signature_statuses(
        &mut self,
        signatures: &[String],
    ) -> Result<Vec<Option<TransactionConfirmationStatus>>, SourceError> {
        let sigs: Vec<Signature> = signatures
            .iter()
            .map(|s| parse_sig(s))
            .collect::<Result<_, _>>()?;
        let mut last_err = None;
        for client in &self.endpoints {
            // `_with_history`: a signature this crate asks about can be arbitrarily old (it is already
            // committed to `settlement_tx`, possibly hours ago), well past the plain
            // `get_signature_statuses` recent-status cache window.
            match tokio::time::timeout(
                RPC_REQUEST_TIMEOUT,
                client.get_signature_statuses_with_history(&sigs),
            )
            .await
            {
                Ok(Ok(resp)) => {
                    return Ok(resp
                        .value
                        .into_iter()
                        .map(|s| s.and_then(|s| s.confirmation_status))
                        .collect());
                }
                Ok(Err(e)) => last_err = Some(e.to_string()),
                Err(_) => last_err = Some("getSignatureStatuses timed out".to_string()),
            }
        }
        Err(SourceError::Rpc(last_err.unwrap_or_default()))
    }

    async fn current_slot(&mut self) -> Result<u64, SourceError> {
        let mut last_err = None;
        for client in &self.endpoints {
            match tokio::time::timeout(
                RPC_REQUEST_TIMEOUT,
                client.get_slot_with_commitment(CommitmentConfig::finalized()),
            )
            .await
            {
                Ok(Ok(slot)) => return Ok(slot),
                Ok(Err(e)) => last_err = Some(e.to_string()),
                Err(_) => last_err = Some("getSlot timed out".to_string()),
            }
        }
        Err(SourceError::Rpc(last_err.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned per-endpoint outcome for [`failover_get_transaction`]'s unit tests -- no network, no
    /// timeout, just the exact three shapes a real endpoint's [`TxAttempt::attempt`] can produce.
    enum FakeEndpoint {
        Null,
        Body,
        Failed(&'static str),
    }

    fn fake_raw_tx() -> RawTx {
        RawTx {
            message: UiRawMessage {
                header: solana_sdk::message::MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                account_keys: vec![Pubkey::new_unique().to_string()],
                recent_blockhash: Pubkey::new_unique().to_string(),
                instructions: vec![],
                address_table_lookups: None,
                // New field on `UiRawMessage` (API fallout).
                transaction_config: None,
            },
            err: false,
        }
    }

    impl TxAttempt for FakeEndpoint {
        async fn attempt(&self, _sig: &Signature) -> Result<Option<RawTx>, SourceError> {
            match self {
                FakeEndpoint::Null => Ok(None),
                FakeEndpoint::Body => Ok(Some(fake_raw_tx())),
                FakeEndpoint::Failed(msg) => Err(SourceError::Rpc(msg.to_string())),
            }
        }
    }

    /// RED before the fix: a null body from the PRIMARY endpoint must not short-circuit the call -- the SECONDARY,
    /// which actually has the body, must still be tried. The unfixed code returned `Ok(None)` the instant the first
    /// endpoint answered null, exactly the class of failure Agave's own Bigtable-null-on-transient-error mapping
    /// produces.
    #[tokio::test]
    async fn a_null_primary_falls_through_to_a_secondary_that_has_the_body() {
        let endpoints = vec![FakeEndpoint::Null, FakeEndpoint::Body];
        let sig = Signature::default();
        let result = failover_get_transaction(&endpoints, &sig).await.unwrap();
        assert!(
            result.is_some(),
            "a null primary must not stop the secondary (which has the real body) from being tried"
        );
    }

    /// `Ok(None)` is the verdict only once every configured endpoint agrees -- never on the first null.
    #[tokio::test]
    async fn none_is_returned_only_once_every_endpoint_reports_null() {
        let endpoints = vec![FakeEndpoint::Null, FakeEndpoint::Null];
        let sig = Signature::default();
        let result = failover_get_transaction(&endpoints, &sig).await.unwrap();
        assert!(result.is_none());
    }

    /// A null primary plus a secondary that itself errors (rather than agreeing null, or finding the
    /// body) must not silently resolve to `Ok(None)` -- an inconclusive answer propagates as an error
    /// rather than being read as "this signature does not exist".
    #[tokio::test]
    async fn a_null_primary_and_an_erroring_secondary_is_an_error_not_a_silent_none() {
        let endpoints = vec![FakeEndpoint::Null, FakeEndpoint::Failed("secondary down")];
        let sig = Signature::default();
        let err = failover_get_transaction(&endpoints, &sig)
            .await
            .expect_err("an erroring secondary must not resolve to Ok(None)");
        assert!(matches!(err, SourceError::Rpc(_)));
    }
}
