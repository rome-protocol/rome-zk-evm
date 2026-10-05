//! Reads the settlement program's own `root`/`pending(batch)`/`exit_config` accounts — the three facts
//! [`crate::core::attempt_exit`] needs before it ever calls the verifier: the newest Final batch
//! (`root.head_final_batch`), that batch's `state_root`/`last_block` (`pending`), and the portal address
//! `ProveExit` itself binds from (`exit_config.exit_portal` — never a caller/message field).
//!
//! [`SettlementReader`] is a trait (never a bare RPC call) so the core loop's tests drive it against a
//! scripted fake — the same `Deps`-over-fakes shape `rome-zk-prover::follower`'s own `SnapshotFetch`/
//! `VerifierFetch` traits use. [`RpcSettlementReader`] is the one real, `solana-client`-backed
//! implementation, built from `zk_settlement_client`'s own PDA derivation
//! ([`zk_settlement_client::root_pda`]/[`pending_pda`]/[`exit_config_pda`]) and decoders
//! ([`zk_settlement_client::decode_root_account`]/[`decode_pending_account`]/[`decode_exit_config_account`]).

use solana_program::pubkey::Pubkey;
use zk_settlement_client::{
    ExitConfigAccount, ExitWindowAccount, NullifierPageAccount, PendingAccount, RootAccount,
};

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("account {0} not found")]
    NotFound(Pubkey),
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("decode: {0}")]
    Decode(String),
}

pub trait SettlementReader: Send + Sync {
    fn read_root(&self) -> Result<RootAccount, ReadError>;
    fn read_pending(&self, batch: u64) -> Result<PendingAccount, ReadError>;
    fn read_exit_config(&self) -> Result<ExitConfigAccount, ReadError>;
    /// Reads the `exit_nullifier` page for `page` — `Ok(None)` when the account does not exist yet:
    /// "absent account = bit clear" (no exit on this page has ever been proved), never an
    /// error. [`crate::core::attempt_exit`] calls this BEFORE `read_pending`/`eth_getProof`/`Sender`:
    /// the nullifier bit `ProveExit` writes is the source of truth for whether this
    /// exit is already done.
    fn read_nullifier_page(&self, page: u64) -> Result<Option<NullifierPageAccount>, ReadError>;
    /// Reads the `exit_window` account for `window_index` — `Ok(None)` when the account does not exist
    /// yet: no exit has spent any cap in this window (`spent_cap_units == 0`), never an error.
    /// [`crate::core::attempt_exit`] calls this BEFORE `read_pending`/
    /// `eth_getProof`/`Sender`, right after the amount-vs-whole-cap pre-check, so a window already spent
    /// past its cap never reaches a fetch/verify/send — the on-chain `ExitCapExceeded` (68) classification
    /// still catches the race where two followers (or two polls of one) both pass this read first.
    fn read_exit_window(&self, window_index: u64) -> Result<Option<ExitWindowAccount>, ReadError>;
    /// The current Solana slot — read fresh every poll (never cached), the same commitment-agnostic call
    /// `rome-zk-exit-prover`'s bin used inline before `run::poll_once` lifted it behind this trait so a
    /// `get_slot` failure is testable against a fake (a failure here must skip the
    /// whole attempt round, never fall back to slot `0`, which would send a wrong-window `ProveExit` at a
    /// fee).
    fn read_slot(&self) -> Result<u64, ReadError>;
}

/// The real reader: one blocking `solana-client` `RpcClient`, read at `finalized` commitment —
/// the same commitment level `rome-zk-prover`'s own anchor read uses (every chain-state
/// decision the prover side makes reads FINALIZED, distinct from whatever commitment a sender confirms
/// at), so this crate never acts on a root the network could still reorganize away from.
pub struct RpcSettlementReader {
    pub client: solana_client::rpc_client::RpcClient,
    pub program_id: Pubkey,
    pub chain_id: u64,
}

impl RpcSettlementReader {
    fn get_account_data(&self, pubkey: &Pubkey) -> Result<Vec<u8>, ReadError> {
        self.client
            .get_account_with_commitment(
                pubkey,
                solana_commitment_config::CommitmentConfig::finalized(),
            )
            .map_err(|e| ReadError::Rpc(e.to_string()))?
            .value
            .map(|a| a.data)
            .ok_or(ReadError::NotFound(*pubkey))
    }
}

impl SettlementReader for RpcSettlementReader {
    fn read_root(&self) -> Result<RootAccount, ReadError> {
        let (root_pda, _) = zk_settlement_client::root_pda(&self.program_id, self.chain_id);
        let data = self.get_account_data(&root_pda)?;
        zk_settlement_client::decode_root_account(&data)
            .map_err(|e| ReadError::Decode(format!("{e:?}")))
    }

    fn read_pending(&self, batch: u64) -> Result<PendingAccount, ReadError> {
        let (pending_pda, _) =
            zk_settlement_client::pending_pda(&self.program_id, self.chain_id, batch);
        let data = self.get_account_data(&pending_pda)?;
        zk_settlement_client::decode_pending_account(&data)
            .map_err(|e| ReadError::Decode(format!("{e:?}")))
    }

    fn read_exit_config(&self) -> Result<ExitConfigAccount, ReadError> {
        let (exit_config_pda, _) =
            zk_settlement_client::exit_config_pda(&self.program_id, self.chain_id);
        let data = self.get_account_data(&exit_config_pda)?;
        zk_settlement_client::decode_exit_config_account(&data)
            .map_err(|e| ReadError::Decode(format!("{e:?}")))
    }

    fn read_nullifier_page(&self, page: u64) -> Result<Option<NullifierPageAccount>, ReadError> {
        let (nullifier_pda, _) =
            zk_settlement_client::exit_nullifier_pda(&self.program_id, self.chain_id, page);
        match self.get_account_data(&nullifier_pda) {
            Ok(data) => zk_settlement_client::decode_exit_nullifier_account(&data)
                .map(Some)
                .map_err(|e| ReadError::Decode(format!("{e:?}"))),
            Err(ReadError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_exit_window(&self, window_index: u64) -> Result<Option<ExitWindowAccount>, ReadError> {
        let (exit_window_pda, _) =
            zk_settlement_client::exit_window_pda(&self.program_id, self.chain_id, window_index);
        match self.get_account_data(&exit_window_pda) {
            Ok(data) => zk_settlement_client::decode_exit_window_account(&data)
                .map(Some)
                .map_err(|e| ReadError::Decode(format!("{e:?}"))),
            Err(ReadError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_slot(&self) -> Result<u64, ReadError> {
        self.client
            .get_slot()
            .map_err(|e| ReadError::Rpc(e.to_string()))
    }
}

impl crate::release::AccountReader for RpcSettlementReader {
    fn read_account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, ReadError> {
        match self.get_account_data(key) {
            Ok(data) => Ok(Some(data)),
            Err(ReadError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_accounts(&self, keys: &[Pubkey]) -> Result<Vec<Option<Vec<u8>>>, ReadError> {
        self.client
            .get_multiple_accounts_with_commitment(
                keys,
                solana_commitment_config::CommitmentConfig::finalized(),
            )
            .map(|r| r.value.into_iter().map(|a| a.map(|a| a.data)).collect())
            .map_err(|e| ReadError::Rpc(e.to_string()))
    }
}
