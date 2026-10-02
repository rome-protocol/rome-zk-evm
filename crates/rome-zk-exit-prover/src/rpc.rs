//! JSON-RPC reads against the L2 verifier node (`--verifier-rpc`): `eth_getLogs` (the portal's
//! `ExitInitiated` events) and `eth_getProof` (the account + storage proof this crate's own local MPT
//! verify runs against). Plain JSON-RPC over `ureq` — the exact shape
//! `rome-zk-prover-input::verifier::rpc_call` uses (`crates/rome-zk-prover-input/src/verifier.rs:72`)
//! — never `alloy-provider`: this crate needs no reth/alloy type at all, only hex-string JSON fields, so
//! it carries none of that dependency weight.
//!
//! [`VerifierRpc`] is a trait (not a bare function) so [`crate::core::attempt_exit`]'s tests drive it
//! against a scripted fake instead of a live node — the `Deps`-over-fakes pattern `rome-zk-prover::follower`
//! already uses.

use std::time::Duration;

use serde::Deserialize;

/// Mirrors `rome-zk-prover-input::verifier`'s own request timeout constant — generous, but bounded so a
/// hung tunnel to the verifier fails loud rather than blocking the follow loop forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum VerifierError {
    #[error("verifier RPC {method} at {url}: {source}")]
    Transport {
        url: String,
        method: &'static str,
        #[source]
        source: Box<ureq::Error>,
    },
    #[error("verifier RPC {method}: malformed JSON response: {source}")]
    BadResponseJson {
        method: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("verifier RPC {method} returned a JSON-RPC error: {error}")]
    RpcError { method: &'static str, error: String },
    #[error("verifier RPC {method}: decode result: {source}")]
    Decode {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl VerifierError {
    /// A transport-level failure a caller may retry unchanged (the connection, a timeout, a torn
    /// response body) — never a JSON-RPC error or a decode failure, which are facts about the data.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport { .. } | Self::BadResponseJson { .. })
    }

    /// The verifier refused an `eth_getProof` because the requested block is outside its own
    /// `--rpc.eth-proof-window` — this is the ONLY JSON-RPC error string this crate
    /// classifies by content rather than transport shape, because the verifier reports it as a plain
    /// `RpcError`, not a distinguishable JSON-RPC error code. Matched by substring against the reth
    /// verifier's own message (`reth`'s `HistoryTooOld`/proof-window rejection text), never by parsing a
    /// structured field the node does not send.
    pub fn is_proof_window_exceeded(&self) -> bool {
        match self {
            Self::RpcError { error, .. } => {
                error.contains("distance to target block exceeds maximum proof window")
                    || error.contains("exceeds maximum proof window")
            }
            _ => false,
        }
    }
}

fn rpc_call(
    url: &str,
    method: &'static str,
    params: serde_json::Value,
) -> Result<serde_json::Value, VerifierError> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let response: serde_json::Value = ureq::post(url)
        .timeout(REQUEST_TIMEOUT)
        .set("content-type", "application/json")
        .send_json(body)
        .map_err(|e| VerifierError::Transport {
            url: url.to_string(),
            method,
            source: Box::new(e),
        })?
        .into_json()
        .map_err(|source| VerifierError::BadResponseJson { method, source })?;
    if let Some(error) = response.get("error") {
        return Err(VerifierError::RpcError {
            method,
            error: error
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string()),
        });
    }
    Ok(response
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

fn block_number_hex(number: u64) -> String {
    format!("0x{number:x}")
}

/// One `eth_getProof` storage-proof entry (`storageProof[i]`).
#[derive(Debug, Clone, Deserialize)]
pub struct StorageProofEntry {
    pub key: String,
    pub value: String,
    pub proof: Vec<String>,
}

/// The `eth_getProof` result shape this crate reads: the portal's account proof plus exactly one
/// requested storage-slot proof (`storageProof[0]`) — the same fields `fixtures/exit/anvil_getProof.json`
/// carries (a committed fixture; this module's tests parse it directly, quoted verbatim).
#[derive(Debug, Clone, Deserialize)]
pub struct GetProofResult {
    pub address: String,
    pub balance: String,
    pub nonce: String,
    #[serde(rename = "storageHash")]
    pub storage_hash: String,
    #[serde(rename = "codeHash")]
    pub code_hash: String,
    #[serde(rename = "accountProof")]
    pub account_proof: Vec<String>,
    #[serde(rename = "storageProof")]
    pub storage_proof: Vec<StorageProofEntry>,
}

/// One `eth_getLogs` entry — the fields this crate's own `ExitInitiated` decoder needs
/// (`fixtures/exit/anvil_exit_logs.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct LogEntry {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
    #[serde(rename = "logIndex")]
    pub log_index: String,
}

/// The subset of `eth_getBlockByNumber`'s response this crate reads (`fixtures/exit/anvil_block_by_number.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct BlockByNumber {
    pub number: String,
    #[serde(rename = "stateRoot")]
    pub state_root: String,
    pub hash: String,
}

/// The RPC calls the exit prover's follow loop needs against the L2 verifier — a trait so
/// [`crate::core::attempt_exit`]'s tests drive it against a scripted fake.
pub trait VerifierRpc: Send + Sync {
    /// `eth_getLogs({address, fromBlock, toBlock: "latest"})` — every `ExitInitiated` log from
    /// `from_block` onward.
    fn eth_get_logs(
        &self,
        portal_address: &str,
        from_block: u64,
    ) -> Result<Vec<LogEntry>, VerifierError>;

    /// `eth_getProof(address, [slot], <hex block>)`.
    fn eth_get_proof(
        &self,
        address: &str,
        slot: &str,
        block: u64,
    ) -> Result<GetProofResult, VerifierError>;

    /// `eth_getBlockByNumber(<hex block>, false)`.
    fn eth_get_block_by_number(&self, block: u64) -> Result<BlockByNumber, VerifierError>;
}

/// The real, `ureq`-backed verifier client.
pub struct HttpVerifierRpc {
    pub url: String,
}

impl VerifierRpc for HttpVerifierRpc {
    fn eth_get_logs(
        &self,
        portal_address: &str,
        from_block: u64,
    ) -> Result<Vec<LogEntry>, VerifierError> {
        let result = rpc_call(
            &self.url,
            "eth_getLogs",
            serde_json::json!([{
                "address": portal_address,
                "fromBlock": block_number_hex(from_block),
                "toBlock": "latest",
            }]),
        )?;
        serde_json::from_value(result).map_err(|source| VerifierError::Decode {
            method: "eth_getLogs",
            source,
        })
    }

    fn eth_get_proof(
        &self,
        address: &str,
        slot: &str,
        block: u64,
    ) -> Result<GetProofResult, VerifierError> {
        let result = rpc_call(
            &self.url,
            "eth_getProof",
            serde_json::json!([address, [slot], block_number_hex(block)]),
        )?;
        serde_json::from_value(result).map_err(|source| VerifierError::Decode {
            method: "eth_getProof",
            source,
        })
    }

    fn eth_get_block_by_number(&self, block: u64) -> Result<BlockByNumber, VerifierError> {
        let result = rpc_call(
            &self.url,
            "eth_getBlockByNumber",
            serde_json::json!([block_number_hex(block), false]),
        )?;
        serde_json::from_value(result).map_err(|source| VerifierError::Decode {
            method: "eth_getBlockByNumber",
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_window_exceeded_is_recognized_by_message() {
        let e = VerifierError::RpcError {
            method: "eth_getProof",
            error: "distance to target block exceeds maximum proof window".to_string(),
        };
        assert!(e.is_proof_window_exceeded());
        assert!(!e.is_transient());
    }

    #[test]
    fn an_unrelated_rpc_error_is_not_proof_window_exceeded() {
        let e = VerifierError::RpcError {
            method: "eth_getProof",
            error: "header not found".to_string(),
        };
        assert!(!e.is_proof_window_exceeded());
    }
}
