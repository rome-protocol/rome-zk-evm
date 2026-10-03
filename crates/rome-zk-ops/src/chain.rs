//! The chain, as far as the commands need it: read an account, read the slot, read an EVM genesis, send one
//! instruction. The commands are generic over this trait so their tests run over a fake that records what it was
//! asked to send. [`RpcChain`] is the real one and sends only through `rome-zk-solana-sender`, as V1.

use crate::keys::{self, Signers};
use rome_zk_solana_sender::{compat, ConfirmCommitment, SendTuning, Sender};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use std::time::Duration;

/// Compute and account limits for operator transactions. Operator transactions pay no priority fee.
pub const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
/// Enough for the settlement and inbox programs plus the accounts a command touches.
pub const LOADED_ACCOUNTS_DATA_SIZE_LIMIT: u32 = 8 * 1024 * 1024;

pub fn send_tuning() -> SendTuning {
    SendTuning {
        compute_unit_limit: COMPUTE_UNIT_LIMIT,
        loaded_accounts_data_size_limit: LOADED_ACCOUNTS_DATA_SIZE_LIMIT,
        priority_fee_micro_lamports: 0,
        max_priority_fee_micro_lamports: 0,
        confirm_timeout: Duration::from_secs(60),
        confirm_commitment: ConfirmCommitment::Confirmed,
        status_poll_interval: rome_zk_solana_sender::DEFAULT_STATUS_POLL_INTERVAL,
    }
}

/// An EVM genesis block's hash and state root.
pub type Genesis = ([u8; 32], [u8; 32]);

#[allow(async_fn_in_trait)]
pub trait Chain {
    /// `Ok(None)` is an account that does not exist. `Err` is a failed request of any kind, and a caller must
    /// never read it as "does not exist".
    async fn account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, String>;
    async fn slot(&self) -> Result<u64, String>;
    /// Block 0 of an EVM chain: its hash and its state root.
    async fn genesis(&self, evm_rpc: &str) -> Result<Genesis, String>;
    /// Sends `ixs` as one V1 transaction, signed by `signers`, and waits for it to confirm. Returns the signature.
    async fn send(&self, ixs: &[Instruction], signers: &Signers) -> Result<String, String>;
}

pub struct RpcChain {
    url: String,
    rpc: solana_client::nonblocking::rpc_client::RpcClient,
}

impl RpcChain {
    pub fn new(url: String) -> Self {
        let rpc =
            solana_client::nonblocking::rpc_client::RpcClient::new_with_timeout_and_commitment(
                url.clone(),
                rome_zk_solana_sender::RPC_REQUEST_TIMEOUT,
                solana_commitment_config::CommitmentConfig::confirmed(),
            );
        Self { url, rpc }
    }
}

fn hex32(v: &serde_json::Value, field: &str) -> Result<[u8; 32], String> {
    let s = v[field]
        .as_str()
        .ok_or_else(|| format!("the EVM RPC's block 0 has no {field}"))?;
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| format!("block 0 {field} is not hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("block 0 {field} is not 32 bytes"))
}

impl Chain for RpcChain {
    async fn account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        self.rpc
            .get_account_with_commitment(
                &compat::to_v1_pubkey(key),
                solana_commitment_config::CommitmentConfig::confirmed(),
            )
            .await
            .map(|r| r.value.map(|a| a.data))
            .map_err(|e| e.to_string())
    }

    async fn slot(&self) -> Result<u64, String> {
        self.rpc.get_slot().await.map_err(|e| e.to_string())
    }

    async fn genesis(&self, evm_rpc: &str) -> Result<Genesis, String> {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x0", false]});
        let resp: serde_json::Value = reqwest::Client::new()
            .post(evm_rpc)
            .timeout(Duration::from_secs(20))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("the EVM RPC request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("the EVM RPC answer is not JSON: {e}"))?;
        let r = &resp["result"];
        Ok((hex32(r, "hash")?, hex32(r, "stateRoot")?))
    }

    async fn send(&self, ixs: &[Instruction], signers: &Signers) -> Result<String, String> {
        let cosigners = signers.cosigners.iter().map(keys::copy).collect();
        let sender =
            rome_zk_solana_sender::RpcSender::new(self.url.clone(), keys::copy(&signers.payer))
                .with_cosigners(cosigners);
        sender
            .send_and_confirm(ixs, send_tuning())
            .await
            .map(|sig| sig.to_string())
            .map_err(|e| e.to_string())
    }
}

/// For `--dry-run` where no network is wanted: every read fails by name and nothing can be sent. The commands
/// take a failed read in a dry run as "skip the check that needed it".
pub struct OfflineChain;

const OFFLINE: &str = "offline: this run has no RPC";

impl Chain for OfflineChain {
    async fn account(&self, _key: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        Err(OFFLINE.to_string())
    }
    async fn slot(&self) -> Result<u64, String> {
        Err(OFFLINE.to_string())
    }
    async fn genesis(&self, _evm_rpc: &str) -> Result<Genesis, String> {
        Err(OFFLINE.to_string())
    }
    async fn send(&self, _ixs: &[Instruction], _signers: &Signers) -> Result<String, String> {
        Err(OFFLINE.to_string())
    }
}
