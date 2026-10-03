//! The fake chain every command test runs over. It answers from a table, and it records every instruction it is
//! asked to send, so a test can say "a dry run sent nothing" by looking at the record.

use crate::chain::{Chain, Genesis};
use crate::keys::Signers;
use solana_keypair::Keypair;
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct FakeChain {
    pub accounts: HashMap<Pubkey, Result<Option<Vec<u8>>, String>>,
    pub slot: Result<u64, String>,
    pub genesis: Result<Genesis, String>,
    pub send_result: Result<String, String>,
    /// Every instruction sent, in order, across all transactions.
    pub sent: Mutex<Vec<Instruction>>,
    /// How many instructions each transaction carried.
    pub tx_sizes: Mutex<Vec<usize>>,
}

impl Default for FakeChain {
    fn default() -> Self {
        Self {
            accounts: HashMap::new(),
            slot: Ok(1_000),
            genesis: Ok(([1u8; 32], [2u8; 32])),
            send_result: Ok("FakeSignature1111".to_string()),
            sent: Mutex::new(Vec::new()),
            tx_sizes: Mutex::new(Vec::new()),
        }
    }
}

impl FakeChain {
    pub fn with(mut self, key: Pubkey, data: Vec<u8>) -> Self {
        self.accounts.insert(key, Ok(Some(data)));
        self
    }
    pub fn failing(mut self, key: Pubkey, why: &str) -> Self {
        self.accounts.insert(key, Err(why.to_string()));
        self
    }
    pub fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
    /// How many transactions were sent.
    pub fn tx_count(&self) -> usize {
        self.tx_sizes.lock().unwrap().len()
    }
}

impl Chain for FakeChain {
    async fn account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        self.accounts.get(key).cloned().unwrap_or(Ok(None))
    }
    async fn slot(&self) -> Result<u64, String> {
        self.slot.clone()
    }
    async fn genesis(&self, _evm_rpc: &str) -> Result<Genesis, String> {
        self.genesis.clone()
    }
    async fn send(&self, ixs: &[Instruction], _signers: &Signers) -> Result<String, String> {
        self.sent.lock().unwrap().extend(ixs.iter().cloned());
        self.tx_sizes.lock().unwrap().push(ixs.len());
        self.send_result.clone()
    }
}

pub fn program() -> Pubkey {
    Pubkey::new_from_array([9u8; 32])
}

pub fn inbox() -> Pubkey {
    Pubkey::new_from_array([8u8; 32])
}

pub fn key_pubkey(k: &Keypair) -> Pubkey {
    crate::keys::pubkey(k)
}

/// Writes a fresh keypair to a unique file under the system temp dir; returns the key and the path. The caller
/// removes the file with [`remove`].
pub fn key_file(tag: &str) -> (Keypair, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let k = Keypair::new_from_array([(n % 250) as u8 + 3; 32]);
    let path = std::env::temp_dir().join(format!(
        "rome-zk-ops-test-{}-{}-{tag}.json",
        std::process::id(),
        n
    ));
    let json = serde_json::to_string(&k.to_bytes().to_vec()).unwrap();
    std::fs::write(&path, json).unwrap();
    (k, path)
}

pub fn remove(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// A root account with the given authority and heads; every other field is zero.
pub fn root_bytes(chain_id: u64, authority: Pubkey, head_pending: u64, head_final: u64) -> Vec<u8> {
    rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
        chain_id,
        number: 0,
        parent_hash: [0; 32],
        state_root: [0; 32],
        block_hash: [0; 32],
        updates: 0,
        profile: 0,
        challenge_window_slots: 100,
        prove_window_slots: 100,
        proving_policy: 1,
        poster_bond: 0,
        exit_cap_per_window: 0,
        authority: authority.to_bytes(),
        head_pending_batch: head_pending,
        head_final_batch: head_final,
        pending_count: 0,
        max_pending: 10,
    })
    .to_vec()
}

pub fn chain_config_bytes(
    chain_id: u64,
    deposit: u64,
    refunded: bool,
    posted: u32,
    max_drift_secs: Option<u64>,
) -> Vec<u8> {
    use rome_zk_layouts::chain_config;
    let mut d = vec![0u8; chain_config::LEN_V2];
    chain_config::write(
        &mut d,
        &chain_config::ChainConfigFields {
            chain_id,
            reserved: false,
            deposit_lamports: deposit,
            deposit_refunded: refunded,
            registered_slot: 5,
            posted_batches: posted,
            fee_base_lamports: 0,
            fee_bps: 0,
            max_drift_secs,
        },
    );
    if max_drift_secs.is_none() {
        d.truncate(chain_config::LEN_V1);
    }
    d
}

pub fn exit_config_bytes(chain_id: u64, pending_mask: u8, activation_slot: u64) -> Vec<u8> {
    rome_zk_layouts::exit::exit_config::write(
        &rome_zk_layouts::exit::exit_config::ExitConfigFields {
            chain_id,
            exit_portal: [0; 20],
            bridge_program: [0; 32],
            pending_exit_portal: [0xAA; 20],
            pending_bridge_program: [0; 32],
            pending_exit_cap: 1_000,
            pending_poster_bond: 0,
            activation_slot,
            pending_mask,
        },
    )
    .to_vec()
}
