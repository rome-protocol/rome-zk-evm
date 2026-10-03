//! One consistent chain snapshot for the poster: every
//! prover-side chain decision — is the vkey of record still active, is the head where we think it is,
//! is the inbox batch finalized or abandoned — comes from ONE `getMultipleAccounts` call at FINALIZED
//! commitment, on the prover's own read client (never the sender's CONFIRMED client, see
//! `rome-zk-solana-sender`'s own module doc). [`anchor`] is that one call plus every named refusal the
//! state machine needs before a proof is ever built or a send is ever attempted.

use solana_program::pubkey::Pubkey;

use crate::config::VkeyOfRecord;

/// Re-exported so callers do not need `zk_inbox_client`'s own import path.
pub type BatchAccount = zk_inbox_client::BatchAccount;

/// One FINALIZED snapshot of every account the poster's next decision depends on.
#[derive(Debug, Clone)]
pub struct Anchor {
    pub root: zk_settlement_client::RootAccount,
    pub registry: zk_settlement_client::RegistryAccount,
    pub chain_config: zk_settlement_client::ChainConfigAccount,
    pub cursor_next_batch: u64,
    /// `None` at the genesis sentinel (`candidate_batch == 1`, predecessor batch 0 was never a real
    /// pending PDA — `predecessor_state` in `settle.rs` reads the root account's own fields there
    /// instead, and so does this crate's `publics::Anchor`/poster).
    pub pred: Option<zk_settlement_client::PendingAccount>,
    /// The inbox batch account for `candidate_batch` (== `head_pending_batch + 1`), already confirmed
    /// finalized — `None` is never returned from [`anchor`]; an absent or not-yet-finalized batch is
    /// always a refusal instead (see [`AnchorError`]).
    pub inbox_batch_head_plus_1: BatchAccount,
    /// `global_config` (treasury, registry authority) — read in this same snapshot rather than a second
    /// round trip (the CLI used to fetch it separately after `anchor()` returned,
    /// which violated the one-snapshot rule).
    pub global_config: zk_settlement_client::GlobalConfigAccount,
    /// Every account `post_root_proved_ix`'s own `post_root_accounts` will load for the candidate
    /// batch's send, in that function's own order, plus the settlement program's own executable account
    /// and its `ProgramData` — the live loaded-accounts preflight's input
    /// (`crate::poster::preflight_loaded_accounts_data_size_limit`): fee payer/authority
    /// (assumed `0` — a plain system-owned wallet has no account data of its own), root, the candidate
    /// batch's own pending PDA (`0`, not yet created), the predecessor's pending PDA (`0` at the genesis
    /// sentinel), registry, the inbox batch account, chain_config, global_config, treasury (assumed `0` —
    /// a system wallet on every chain so far; a treasury that holds account data is legal for the program
    /// and would be under-counted, absorbed only by the ≥ 11 KB of slack in the 32 KiB page rounding — a
    /// bound, stated as such), `system_program` (its live length, read from this same snapshot), the settlement
    /// program's own account (always 36 B, a loader-v3 "Program" record), and its `ProgramData` (the chain's real
    /// program binary size — the dominant term).
    pub account_data_lens: Vec<usize>,
    /// The slot this snapshot's `getMultipleAccounts` call was answered at.
    pub slot: u64,
}

/// Every refusal [`anchor`] can return, by name.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AnchorError {
    #[error("root account {0} not found")]
    RootMissing(Pubkey),
    #[error("root account decode failed: {0}")]
    RootDecode(String),
    /// The chain head moved past our job → re-anchor: `candidate_batch` no longer equals
    /// `root.head_pending_batch + 1`. Covers both a batch already posted (by us, on a retry, or in
    /// principle by someone else) and this job simply being stale.
    #[error(
        "head ahead: candidate batch {candidate_batch} != head_pending_batch {head_pending_batch} + 1"
    )]
    HeadAhead {
        candidate_batch: u64,
        head_pending_batch: u64,
        /// The SAME snapshot's own `root.head_final_batch`: lets a caller
        /// tell "already posted AND already final" (a restart between a post and its own finalize sweep,
        /// `candidate_batch <= head_final_batch`) apart from "posted but not yet final" without a second
        /// read.
        head_final_batch: u64,
    },
    #[error("registry account {0} not found")]
    RegistryMissing(Pubkey),
    #[error("registry account decode failed: {0}")]
    RegistryDecode(String),
    /// No registry entry `(BN254, PLONK, vkey.program_vk)` under layout 1 with `activation_slot <= slot`
    /// and not retired — checked BEFORE any input is built (the decoded-proof-vs-record
    /// check runs separately, later in the pipeline; this is the REGISTRY-vs-record check).
    #[error("vkey of record 0x{program_vk} is not an active layout-1 registry entry: {reason}")]
    VkeyNotActive { program_vk: String, reason: String },
    #[error("chain_config account {0} not found")]
    ChainConfigMissing(Pubkey),
    #[error("chain_config account decode failed: {0}")]
    ChainConfigDecode(String),
    #[error("batch_cursor account {0} not found")]
    CursorMissing(Pubkey),
    #[error("batch_cursor account decode failed: {0}")]
    CursorDecode(String),
    #[error("predecessor pending account decode failed: {0}")]
    PredecessorDecode(String),
    #[error("predecessor pending account {0} not found (and candidate batch is not the genesis sentinel)")]
    PredecessorMissing(Pubkey),
    /// Retry: the inbox batch account for `candidate_batch` exists but its own `finalized` flag is
    /// still `false`, OR the account is absent and the cursor has not yet passed it (nothing to prove
    /// yet). `head_pending_batch`/`cursor_next_batch` (from this SAME snapshot) let a caller compute
    /// `batches_behind = cursor_next_batch - 1 - head_pending_batch` and
    /// distinguish "genuinely idle" (`batches_behind == 0`, the account is absent because it was never
    /// opened) from "busy" (a batch is open but not yet finalized) without a second read.
    #[error("inbox batch {batch} is not finalized yet")]
    InboxNotFinalizedYet {
        batch: u64,
        head_pending_batch: u64,
        cursor_next_batch: u64,
    },
    /// No chain write, alarm + stop. The inbox batch PDA for `candidate_batch` is absent
    /// AND the inbox's own cursor has already moved past it in the SAME snapshot — someone ran
    /// `AbandonBatch` for it (the batcher never does); this id will never be finalized.
    #[error("inbox batch {batch} is abandoned (absent, cursor already past it)")]
    AbandonedInboxBatch { batch: u64 },
    #[error("inbox batch account {0} decode failed: {1}")]
    InboxDecode(Pubkey, String),
    #[error("global_config account {0} not found")]
    GlobalConfigMissing(Pubkey),
    #[error("global_config account decode failed: {0}")]
    GlobalConfigDecode(String),
    /// The settlement program account itself is missing from the
    /// snapshot — cannot happen for a program this chain is actually calling, but never assumed.
    #[error("settlement program account {0} not found")]
    SettlementProgramMissing(Pubkey),
    /// The settlement program's own account is not a valid loader-v3 "Program" record (36 bytes: a
    /// 4-byte little-endian enum tag of `2`, then the 32-byte `ProgramData` address) — this prover only
    /// ever targets an upgradeable-loader-owned program.
    #[error("settlement program account {0} is not a valid upgradeable-loader Program record")]
    SettlementProgramNotUpgradeable(Pubkey),
    /// The `ProgramData` address embedded in the settlement program's own account disagrees with the
    /// address this crate derives via `bpf_loader_upgradeable::get_program_data_address` — the two must
    /// always agree for a genuine loader-v3 program; a mismatch means the configured
    /// `settlement_program_id` is not what it claims to be.
    #[error(
        "settlement program {program_id}'s own ProgramData address {got} disagrees with the derived \
         address {expected}"
    )]
    SettlementProgramDataAddressMismatch {
        program_id: Pubkey,
        expected: Pubkey,
        got: Pubkey,
    },
    #[error("settlement ProgramData account {0} not found")]
    SettlementProgramDataMissing(Pubkey),
    /// A transient failure of the snapshot read itself — a dropped
    /// connection, a rate limit, a timed-out RPC call — never a panic and never confused with any of
    /// the refusals above, which all mean "the snapshot read cleanly but the chain says no". The
    /// follower loop retries the same candidate on this (`RetryReason::TransientFetchError`) rather
    /// than halting.
    #[error("snapshot fetch: {0}")]
    Fetch(#[from] FetchError),
}

/// A [`SnapshotFetch::get_multiple_accounts`] call itself failed — transport-level, never a decoded
/// on-chain fact. Carries a plain description rather than a specific transport error type so this
/// trait stays independent of which Solana client generation a real implementation uses.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// `UpgradeableLoaderState::Program`'s own on-chain encoding: a 4-byte little-endian enum tag (`2`)
/// followed by the 32-byte `ProgramData` address — 36 bytes total, no other shape is valid (this is also
/// exactly why "program account = 36 B" is a fixed, environment-independent fact this crate's own tests pin).
/// Hand-decoded rather than pulling in a bincode dependency for one four-field enum this crate only ever reads one
/// variant of.
fn program_account_programdata_address(data: &[u8]) -> Option<Pubkey> {
    if data.len() != 36 {
        return None;
    }
    let tag = u32::from_le_bytes(data[0..4].try_into().expect("checked len above"));
    if tag != 2 {
        return None;
    }
    Some(Pubkey::new_from_array(
        data[4..36].try_into().expect("checked len above"),
    ))
}

/// The one round trip [`anchor`] makes. `RpcClient::get_multiple_accounts_with_commitment` at
/// FINALIZED implements this for real (the CLI); this module's own tests use a counting fake.
pub trait SnapshotFetch {
    /// Returns `(slot, values)` on success — `values` in the same order as `keys`, `None` for a missing
    /// account, exactly the shape a real `getMultipleAccounts` RPC response carries (`context.slot` +
    /// `value`). `Err(FetchError)` for a transient transport failure — never a panic; a
    /// real implementation (`RpcFetch`, the CLI) maps its own client's error into this.
    #[allow(clippy::type_complexity)]
    fn get_multiple_accounts(
        &mut self,
        keys: &[Pubkey],
    ) -> Result<(u64, Vec<Option<Vec<u8>>>), FetchError>;

    /// The fee payer's own lamport balance — reporting only (`rome_zk_prover_payer_
    /// lamports`), never a decision input: unlike every account in [`Self::get_multiple_accounts`]'s own
    /// snapshot, this is deliberately a SEPARATE read (a plain `getBalance`, not folded into the one
    /// FINALIZED decision snapshot the one-snapshot rule requires) since a stale or slightly-out-of-band balance
    /// reading changes no refusal this crate makes. Defaults to `Ok(0)` so a fake that does not care
    /// about this gauge need not implement it; the CLI's real `RpcFetch` overrides it.
    fn payer_lamports(&mut self, _payer: &Pubkey) -> Result<u64, FetchError> {
        Ok(0)
    }
}

/// Builds the one FINALIZED snapshot the poster needs to attempt `candidate_batch`:
/// ONE [`SnapshotFetch::get_multiple_accounts`] call for `[root, registry, chain_config, cursor,
/// predecessor pending, inbox batch]`, decoded and cross-checked. `candidate_batch` is the batch id the
/// caller intends to post — from a prior anchor's `root.head_pending_batch + 1` in the follower loop, or
/// `--batch N` on the CLI (the `--once` contract) — never rediscovered here, so the
/// predecessor's PDA is known up front and the whole snapshot fits one call.
pub fn anchor(
    fetch: &mut impl SnapshotFetch,
    settlement_program: &Pubkey,
    inbox_program: &Pubkey,
    chain_id: u64,
    candidate_batch: u64,
    vkey: &VkeyOfRecord,
) -> Result<Anchor, AnchorError> {
    let (root_pda, _) = zk_settlement_client::root_pda(settlement_program, chain_id);
    let (registry_pda, _) = zk_settlement_client::registry_pda(settlement_program, chain_id);
    let (chain_config_pda, _) =
        zk_settlement_client::chain_config_pda(settlement_program, chain_id);
    let (cursor_pda, _) = zk_inbox_client::cursor_pda(inbox_program, settlement_program, chain_id);
    let predecessor_batch = candidate_batch.saturating_sub(1);
    let (pred_pda, _) =
        zk_settlement_client::pending_pda(settlement_program, chain_id, predecessor_batch);
    let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
        inbox_program,
        settlement_program,
        chain_id,
        candidate_batch,
    );
    let (global_config_pda, _) = zk_settlement_client::global_config_pda(settlement_program);
    let programdata_pda = zk_settlement_client::program_data_pda(settlement_program);

    let keys = [
        root_pda,
        registry_pda,
        chain_config_pda,
        cursor_pda,
        pred_pda,
        inbox_batch_pda,
        global_config_pda,
        solana_system_interface::program::id(),
        *settlement_program,
        programdata_pda,
    ];
    let (slot, mut accounts) = fetch.get_multiple_accounts(&keys)?;
    assert_eq!(
        accounts.len(),
        keys.len(),
        "SnapshotFetch::get_multiple_accounts must return exactly one entry per key, in order"
    );
    let programdata_d = accounts.pop().unwrap();
    let program_d = accounts.pop().unwrap();
    // The native system program is a real account (21 B, owned by the native loader); the
    // loader charges `64 + len` for it like any other key, so its length is read, never assumed.
    let system_program_len = accounts.pop().unwrap().map(|d| d.len()).unwrap_or(0);
    let global_config_d = accounts.pop().unwrap();
    let inbox_d = accounts.pop().unwrap();
    let pred_d = accounts.pop().unwrap();
    let cursor_d = accounts.pop().unwrap();
    let chain_config_d = accounts.pop().unwrap();
    let registry_d = accounts.pop().unwrap();
    let root_d = accounts.pop().unwrap();

    let root_bytes = root_d.ok_or(AnchorError::RootMissing(root_pda))?;
    let root = zk_settlement_client::decode_root_account(&root_bytes)
        .map_err(|e| AnchorError::RootDecode(e.to_string()))?;

    if candidate_batch != root.head_pending_batch + 1 {
        return Err(AnchorError::HeadAhead {
            candidate_batch,
            head_pending_batch: root.head_pending_batch,
            head_final_batch: root.head_final_batch,
        });
    }

    let registry_bytes = registry_d.ok_or(AnchorError::RegistryMissing(registry_pda))?;
    let registry = zk_settlement_client::decode_registry_account(&registry_bytes)
        .map_err(|e| AnchorError::RegistryDecode(e.to_string()))?;

    let active = registry.entries.iter().any(|e| {
        e.curve == rome_zk_layouts::registry::CURVE_BN254
            && e.scheme == rome_zk_layouts::registry::SCHEME_PLONK
            && e.vkey_hash == vkey.program_vk
            && e.layout_id == rome_zk_layouts::registry::LAYOUT_ZISK_V1
            && e.activation_slot <= slot
            && !e.retired
    });
    if !active {
        let reason = if registry
            .entries
            .iter()
            .any(|e| e.vkey_hash == vkey.program_vk && e.retired)
        {
            "entry is retired".to_string()
        } else if registry
            .entries
            .iter()
            .any(|e| e.vkey_hash == vkey.program_vk && e.activation_slot > slot)
        {
            "entry's activation_slot is in the future".to_string()
        } else {
            "no registry entry for this vkey".to_string()
        };
        return Err(AnchorError::VkeyNotActive {
            program_vk: hex::encode(vkey.program_vk),
            reason,
        });
    }

    let chain_config_bytes =
        chain_config_d.ok_or(AnchorError::ChainConfigMissing(chain_config_pda))?;
    let chain_config = zk_settlement_client::decode_chain_config_account(&chain_config_bytes)
        .map_err(|e| AnchorError::ChainConfigDecode(e.to_string()))?;

    let cursor_next_batch = zk_inbox_client::decode_batch_cursor(
        &cursor_d.ok_or(AnchorError::CursorMissing(cursor_pda))?,
    )
    .map_err(|e| AnchorError::CursorDecode(e.to_string()))?
    .next_batch;

    let (pred, pred_len) = if predecessor_batch == 0 {
        (None, 0usize)
    } else {
        let d = pred_d.ok_or(AnchorError::PredecessorMissing(pred_pda))?;
        let len = d.len();
        let acc = zk_settlement_client::decode_pending_account(&d)
            .map_err(|e| AnchorError::PredecessorDecode(e.to_string()))?;
        (Some(acc), len)
    };

    let (inbox_batch_head_plus_1, inbox_len) = match inbox_d {
        None => {
            if cursor_next_batch > candidate_batch {
                return Err(AnchorError::AbandonedInboxBatch {
                    batch: candidate_batch,
                });
            }
            return Err(AnchorError::InboxNotFinalizedYet {
                batch: candidate_batch,
                head_pending_batch: root.head_pending_batch,
                cursor_next_batch,
            });
        }
        Some(d) => {
            let len = d.len();
            let acc = zk_inbox_client::decode_batch_account(&d)
                .map_err(|e| AnchorError::InboxDecode(inbox_batch_pda, e.to_string()))?;
            if !acc.finalized {
                return Err(AnchorError::InboxNotFinalizedYet {
                    batch: candidate_batch,
                    head_pending_batch: root.head_pending_batch,
                    cursor_next_batch,
                });
            }
            (acc, len)
        }
    };

    let global_config_bytes =
        global_config_d.ok_or(AnchorError::GlobalConfigMissing(global_config_pda))?;
    let global_config_len = global_config_bytes.len();
    let global_config = zk_settlement_client::decode_global_config_account(&global_config_bytes)
        .map_err(|e| AnchorError::GlobalConfigDecode(e.to_string()))?;

    let program_bytes =
        program_d.ok_or(AnchorError::SettlementProgramMissing(*settlement_program))?;
    let program_len = program_bytes.len();
    let embedded_programdata = program_account_programdata_address(&program_bytes).ok_or(
        AnchorError::SettlementProgramNotUpgradeable(*settlement_program),
    )?;
    if embedded_programdata != programdata_pda {
        return Err(AnchorError::SettlementProgramDataAddressMismatch {
            program_id: *settlement_program,
            expected: programdata_pda,
            got: embedded_programdata,
        });
    }
    let programdata_len = programdata_d
        .ok_or(AnchorError::SettlementProgramDataMissing(programdata_pda))?
        .len();

    // `post_root_accounts`' own order (zk-settlement-client), plus the executable program account and
    // its ProgramData (SIMD-0186 charges both even though neither is one of that function's own
    // `AccountMeta`s) — see this struct's own field doc for why the three `0`s are assumed rather than
    // fetched.
    let account_data_lens = vec![
        0, // authority / fee payer
        root_bytes.len(),
        0, // the candidate batch's own pending PDA — not yet created
        pred_len,
        registry_bytes.len(),
        inbox_len,
        chain_config_bytes.len(),
        global_config_len,
        0, // treasury — see the field doc: a 0-data system wallet on every chain so far; rides on page slack
        system_program_len,
        program_len,
        programdata_len,
    ];

    Ok(Anchor {
        root,
        registry,
        chain_config,
        global_config,
        cursor_next_batch,
        pred,
        inbox_batch_head_plus_1,
        account_data_lens,
        slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A counting fake `SnapshotFetch`: every call is recorded (count) so `anchor()`'s "exactly one
    /// read per call" premise is directly assertable, never merely assumed from reading
    /// the source.
    #[derive(Default)]
    struct FakeSnapshot {
        slot: u64,
        accounts: HashMap<Pubkey, Vec<u8>>,
        calls: u32,
        /// When `true`, the NEXT call returns `Err` instead of a snapshot, then clears itself — models a
        /// transient RPC failure.
        error_once: bool,
    }
    impl SnapshotFetch for FakeSnapshot {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), FetchError> {
            self.calls += 1;
            if self.error_once {
                self.error_once = false;
                return Err(FetchError("simulated transient RPC error".to_string()));
            }
            Ok((
                self.slot,
                keys.iter().map(|k| self.accounts.get(k).cloned()).collect(),
            ))
        }
    }

    fn encode_root(
        chain_id: u64,
        number: u64,
        state_root: [u8; 32],
        authority: [u8; 32],
        head_pending_batch: u64,
        head_final_batch: u64,
        max_pending: u32,
    ) -> Vec<u8> {
        use rome_zk_layouts::root::*;
        let mut d = vec![0u8; MIN_LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_NUMBER..OFF_NUMBER + 8].copy_from_slice(&number.to_le_bytes());
        d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
        d[OFF_AUTHORITY..OFF_AUTHORITY + 32].copy_from_slice(&authority);
        d[OFF_HEAD_PENDING_BATCH..OFF_HEAD_PENDING_BATCH + 8]
            .copy_from_slice(&head_pending_batch.to_le_bytes());
        d[OFF_HEAD_FINAL_BATCH..OFF_HEAD_FINAL_BATCH + 8]
            .copy_from_slice(&head_final_batch.to_le_bytes());
        d[OFF_MAX_PENDING..OFF_MAX_PENDING + 4].copy_from_slice(&max_pending.to_le_bytes());
        d
    }

    /// `entries`: `(curve, scheme, vkey_hash, layout_id, activation_slot)`. Always encodes the v2
    /// (with-activation-tail) shape so every test can express retirement / future-activation directly.
    fn encode_registry_v2(
        chain_id: u64,
        inbox_program: [u8; 32],
        entries: &[(u8, u8, [u8; 32], u8, u64)],
    ) -> Vec<u8> {
        use rome_zk_layouts::registry::*;
        let mut d = vec![0u8; REGISTRY_LEN_V2];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32].copy_from_slice(&inbox_program);
        d[OFF_COUNT] = entries.len() as u8;
        for (i, (curve, scheme, vkey_hash, layout_id, activation_slot)) in
            entries.iter().enumerate()
        {
            let e = OFF_ENTRIES + i * ENTRY_LEN;
            d[e] = *curve;
            d[e + 1] = *scheme;
            d[e + 2..e + 34].copy_from_slice(vkey_hash);
            d[e + 34] = *layout_id;
            let a = OFF_ACTIVATION + i * ACTIVATION_ENTRY_LEN;
            d[a..a + 8].copy_from_slice(&activation_slot.to_le_bytes());
        }
        d
    }

    fn encode_chain_config_v2(chain_id: u64, max_drift_secs: u64) -> Vec<u8> {
        use rome_zk_layouts::chain_config::*;
        let mut d = vec![0u8; LEN_V2];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8]
            .copy_from_slice(&max_drift_secs.to_le_bytes());
        d
    }

    fn encode_pending(batch: u64, last_block: u64, state_root: [u8; 32], status: u8) -> Vec<u8> {
        use rome_zk_layouts::pending::*;
        let mut d = vec![0u8; PENDING_LEN];
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&batch.to_le_bytes());
        d[OFF_LAST_BLOCK..OFF_LAST_BLOCK + 8].copy_from_slice(&last_block.to_le_bytes());
        d[OFF_STATE_ROOT..OFF_STATE_ROOT + 32].copy_from_slice(&state_root);
        d[OFF_STATUS] = status;
        d
    }

    fn encode_inbox_batch(
        chain_id: u64,
        batch: u64,
        expected_count: u32,
        finalized: bool,
    ) -> Vec<u8> {
        rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
            chain_id,
            batch,
            open_slot: 1,
            expected_count,
            leaves_present: expected_count,
            finalized,
            settlement_program: [0u8; 32],
            authority: [0u8; 32],
            root: [0u8; 32],
            forced_root: [0u8; 32],
            acc: [0u8; 32],
            finalize_cursor: expected_count,
            open_unix_ts: 1,
            deposit: None,
        })
        .to_vec()
    }

    /// A loader-v3 "Program" account's own encoding (36 B: tag `2` LE + the ProgramData address) —
    /// matches [`program_account_programdata_address`] exactly.
    fn encode_program_account(programdata: Pubkey) -> Vec<u8> {
        let mut d = vec![0u8; 36];
        d[0..4].copy_from_slice(&2u32.to_le_bytes());
        d[4..36].copy_from_slice(programdata.as_ref());
        d
    }

    fn encode_global_config(treasury: Pubkey) -> Vec<u8> {
        use rome_zk_layouts::global_config::*;
        let mut d = vec![0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_TREASURY..OFF_TREASURY + 32].copy_from_slice(treasury.as_ref());
        d
    }

    /// Seeds the three accounts added to `anchor()`'s own snapshot: the settlement program's
    /// own (loader-v3) account, its `ProgramData` (a placeholder length — only the real-BPF gate rig
    /// needs the genuine `.so` size), and `global_config`. Every `FakeSnapshot`-driven test that reaches
    /// `anchor()`'s happy path needs this; the ones that refuse earlier (`HeadAhead`, `VkeyNotActive`,
    /// `InboxNotFinalizedYet`, `AbandonedInboxBatch`, …) never read these three, so they don't.
    fn seed_settlement_program_and_config(fetch: &mut FakeSnapshot, settlement_program: Pubkey) {
        let programdata_pda = zk_settlement_client::program_data_pda(&settlement_program);
        fetch
            .accounts
            .insert(settlement_program, encode_program_account(programdata_pda));
        fetch.accounts.insert(programdata_pda, vec![0u8; 1_000]);
        let (global_config_pda, _) = zk_settlement_client::global_config_pda(&settlement_program);
        fetch.accounts.insert(
            global_config_pda,
            encode_global_config(Pubkey::new_unique()),
        );
    }

    fn test_vkey() -> VkeyOfRecord {
        VkeyOfRecord {
            program_vk: [7u8; 32],
            root_c: [8u8; 32],
            elf_sha256: [9u8; 32],
            chain_id: 200_101,
            layout_id: 1,
        }
    }

    /// A fully-wired happy-path fixture: chain_id 200101, candidate batch 1 (genesis sentinel — no
    /// predecessor pending PDA), inbox batch 1 present + finalized, registry carries the test vkey
    /// active from slot 0, cursor at 2 (batch 1 already closed/DA-complete).
    struct Fixture {
        fetch: FakeSnapshot,
        settlement_program: Pubkey,
        inbox_program: Pubkey,
        chain_id: u64,
        candidate_batch: u64,
        vkey: VkeyOfRecord,
    }
    impl Fixture {
        fn happy_path() -> Self {
            let settlement_program = Pubkey::new_unique();
            let inbox_program = Pubkey::new_unique();
            let chain_id = 200_101u64;
            let candidate_batch = 1u64;
            let vkey = test_vkey();
            let mut fetch = FakeSnapshot {
                slot: 100,
                ..Default::default()
            };
            let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program, chain_id);
            fetch.accounts.insert(
                root_pda,
                encode_root(chain_id, 0, [0u8; 32], [1u8; 32], 0, 0, 16),
            );
            let (registry_pda, _) =
                zk_settlement_client::registry_pda(&settlement_program, chain_id);
            fetch.accounts.insert(
                registry_pda,
                encode_registry_v2(
                    chain_id,
                    inbox_program.to_bytes(),
                    &[(
                        rome_zk_layouts::registry::CURVE_BN254,
                        rome_zk_layouts::registry::SCHEME_PLONK,
                        vkey.program_vk,
                        rome_zk_layouts::registry::LAYOUT_ZISK_V1,
                        0,
                    )],
                ),
            );
            let (cc_pda, _) = zk_settlement_client::chain_config_pda(&settlement_program, chain_id);
            fetch
                .accounts
                .insert(cc_pda, encode_chain_config_v2(chain_id, 60));
            let (cursor_pda, _) =
                zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, chain_id);
            fetch.accounts.insert(
                cursor_pda,
                rome_zk_testkit::cursor_account(inbox_program, chain_id, 2).data,
            );
            let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
                &inbox_program,
                &settlement_program,
                chain_id,
                candidate_batch,
            );
            fetch.accounts.insert(
                inbox_batch_pda,
                encode_inbox_batch(chain_id, candidate_batch, 1, true),
            );
            seed_settlement_program_and_config(&mut fetch, settlement_program);
            Fixture {
                fetch,
                settlement_program,
                inbox_program,
                chain_id,
                candidate_batch,
                vkey,
            }
        }

        fn anchor(&mut self) -> Result<Anchor, AnchorError> {
            anchor(
                &mut self.fetch,
                &self.settlement_program,
                &self.inbox_program,
                self.chain_id,
                self.candidate_batch,
                &self.vkey,
            )
        }
    }

    #[test]
    fn happy_path_anchors_at_the_genesis_sentinel_with_exactly_one_read() {
        let mut f = Fixture::happy_path();
        let a = f.anchor().expect("should anchor cleanly");
        assert_eq!(a.root.head_pending_batch, 0);
        assert!(
            a.pred.is_none(),
            "batch 1's predecessor is the genesis sentinel"
        );
        assert_eq!(a.inbox_batch_head_plus_1.batch, 1);
        assert_eq!(a.cursor_next_batch, 2);
        assert_eq!(a.slot, 100);
        assert_eq!(
            f.fetch.calls, 1,
            "anchor() must make exactly one get_multiple_accounts call"
        );
        assert_eq!(
            a.account_data_lens.len(),
            12,
            "post_root_accounts' 10 accounts + program + ProgramData"
        );
    }

    /// RED: a transient snapshot-fetch failure is a named
    /// `AnchorError::Fetch`, never a panic — the follower's own caller turns this into a retry.
    #[test]
    fn a_transient_fetch_failure_is_a_named_error_not_a_panic() {
        let mut f = Fixture::happy_path();
        f.fetch.error_once = true;
        let err = f.anchor().unwrap_err();
        assert!(matches!(err, AnchorError::Fetch(_)), "got {err:?}");
    }

    #[test]
    fn head_ahead_when_candidate_batch_does_not_match_head_pending_plus_one() {
        let mut f = Fixture::happy_path();
        f.candidate_batch = 2; // root still says head_pending_batch == 0
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::HeadAhead {
                    candidate_batch: 2,
                    head_pending_batch: 0,
                    ..
                }
            ),
            "got {err:?}"
        );
        assert_eq!(f.fetch.calls, 1, "still exactly one read, even on refusal");
    }

    #[test]
    fn vkey_not_active_when_the_registry_has_no_entry_for_it() {
        let mut f = Fixture::happy_path();
        f.vkey.program_vk = [0xAAu8; 32]; // not the registered entry's vkey
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::VkeyNotActive { .. }),
            "got {err:?}"
        );
        assert_eq!(f.fetch.calls, 1);
    }

    #[test]
    fn vkey_not_active_when_the_registered_entry_is_retired() {
        let mut f = Fixture::happy_path();
        let (registry_pda, _) =
            zk_settlement_client::registry_pda(&f.settlement_program, f.chain_id);
        f.fetch.accounts.insert(
            registry_pda,
            encode_registry_v2(
                f.chain_id,
                f.inbox_program.to_bytes(),
                &[(
                    rome_zk_layouts::registry::CURVE_BN254,
                    rome_zk_layouts::registry::SCHEME_PLONK,
                    f.vkey.program_vk,
                    rome_zk_layouts::registry::LAYOUT_ZISK_V1,
                    rome_zk_layouts::registry::RETIRED_SLOT,
                )],
            ),
        );
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::VkeyNotActive { ref reason, .. } if reason.contains("retired")),
            "got {err:?}"
        );
    }

    #[test]
    fn vkey_not_active_when_the_registered_entry_activates_in_the_future() {
        let mut f = Fixture::happy_path();
        let (registry_pda, _) =
            zk_settlement_client::registry_pda(&f.settlement_program, f.chain_id);
        f.fetch.accounts.insert(
            registry_pda,
            encode_registry_v2(
                f.chain_id,
                f.inbox_program.to_bytes(),
                &[(
                    rome_zk_layouts::registry::CURVE_BN254,
                    rome_zk_layouts::registry::SCHEME_PLONK,
                    f.vkey.program_vk,
                    rome_zk_layouts::registry::LAYOUT_ZISK_V1,
                    f.fetch.slot + 1, // activates one slot after this snapshot's own slot
                )],
            ),
        );
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::VkeyNotActive { ref reason, .. } if reason.contains("future")),
            "got {err:?}"
        );
    }

    #[test]
    fn inbox_not_finalized_yet_when_the_batch_pda_exists_but_is_not_finalized() {
        let mut f = Fixture::happy_path();
        let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
            &f.inbox_program,
            &f.settlement_program,
            f.chain_id,
            f.candidate_batch,
        );
        f.fetch.accounts.insert(
            inbox_batch_pda,
            encode_inbox_batch(f.chain_id, f.candidate_batch, 1, false),
        );
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::InboxNotFinalizedYet { batch: 1, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inbox_not_finalized_yet_when_the_batch_pda_is_absent_and_the_cursor_has_not_passed_it() {
        let mut f = Fixture::happy_path();
        let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
            &f.inbox_program,
            &f.settlement_program,
            f.chain_id,
            f.candidate_batch,
        );
        f.fetch.accounts.remove(&inbox_batch_pda);
        let (cursor_pda, _) =
            zk_inbox_client::cursor_pda(&f.inbox_program, &f.settlement_program, f.chain_id);
        f.fetch.accounts.insert(
            cursor_pda,
            rome_zk_testkit::cursor_account(f.inbox_program, f.chain_id, 1).data, // cursor still at 1, not past batch 1
        );
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::InboxNotFinalizedYet { batch: 1, .. }),
            "got {err:?}"
        );
    }

    /// An absent inbox PDA AND a cursor already past it is `AbandonedInboxBatch` — alarm
    /// and stop, never a chain write.
    #[test]
    fn abandoned_inbox_batch_when_the_pda_is_absent_and_the_cursor_has_passed_it() {
        let mut f = Fixture::happy_path();
        let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
            &f.inbox_program,
            &f.settlement_program,
            f.chain_id,
            f.candidate_batch,
        );
        f.fetch.accounts.remove(&inbox_batch_pda);
        // cursor is already at 2 (> candidate_batch 1) in the happy-path fixture.
        let err = f.anchor().unwrap_err();
        assert!(
            matches!(err, AnchorError::AbandonedInboxBatch { batch: 1 }),
            "got {err:?}"
        );
        assert_eq!(f.fetch.calls, 1);
    }

    /// A candidate batch past the genesis sentinel reads its predecessor's pending PDA and carries its
    /// `(last_block, state_root)` forward — the same continuity fact `settle.rs::predecessor_state`
    /// reads on-chain.
    #[test]
    fn a_non_genesis_candidate_batch_reads_its_predecessors_pending_account() {
        let settlement_program = Pubkey::new_unique();
        let inbox_program = Pubkey::new_unique();
        let chain_id = 200_101u64;
        let vkey = test_vkey();
        let mut fetch = FakeSnapshot {
            slot: 500,
            ..Default::default()
        };
        let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program, chain_id);
        fetch.accounts.insert(
            root_pda,
            encode_root(chain_id, 60, [3u8; 32], [1u8; 32], 1, 1, 16),
        );
        let (registry_pda, _) = zk_settlement_client::registry_pda(&settlement_program, chain_id);
        fetch.accounts.insert(
            registry_pda,
            encode_registry_v2(
                chain_id,
                inbox_program.to_bytes(),
                &[(
                    rome_zk_layouts::registry::CURVE_BN254,
                    rome_zk_layouts::registry::SCHEME_PLONK,
                    vkey.program_vk,
                    rome_zk_layouts::registry::LAYOUT_ZISK_V1,
                    0,
                )],
            ),
        );
        let (cc_pda, _) = zk_settlement_client::chain_config_pda(&settlement_program, chain_id);
        fetch
            .accounts
            .insert(cc_pda, encode_chain_config_v2(chain_id, 60));
        let (cursor_pda, _) =
            zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, chain_id);
        fetch.accounts.insert(
            cursor_pda,
            rome_zk_testkit::cursor_account(inbox_program, chain_id, 3).data,
        );
        let (pred_pda, _) = zk_settlement_client::pending_pda(&settlement_program, chain_id, 1);
        fetch.accounts.insert(
            pred_pda,
            encode_pending(1, 60, [3u8; 32], rome_zk_layouts::pending::STATUS_FINAL),
        );
        let inbox_batch_pda =
            zk_settlement_client::inbox_batch_pda(&inbox_program, &settlement_program, chain_id, 2);
        fetch
            .accounts
            .insert(inbox_batch_pda, encode_inbox_batch(chain_id, 2, 1, true));
        seed_settlement_program_and_config(&mut fetch, settlement_program);

        let a = anchor(
            &mut fetch,
            &settlement_program,
            &inbox_program,
            chain_id,
            2,
            &vkey,
        )
        .expect("should anchor cleanly");
        let pred = a
            .pred
            .expect("batch 2's predecessor (batch 1) must be read");
        assert_eq!(pred.last_block, 60);
        assert_eq!(pred.state_root, [3u8; 32]);
        assert_eq!(fetch.calls, 1);
        assert_eq!(
            a.account_data_lens.len(),
            12,
            "post_root_accounts' 10 accounts + program + ProgramData"
        );
    }

    /// RED: the derived limit from the LIVE lengths (`solana program show`'s own numbers: ProgramData 543,600 B, a
    /// loader-v3 program account always 36 B, the rest from this workspace's own fixed layout constants) is >= 557,056
    /// (17 pages) — well above the CLI's old 262,144-byte (8-page) literal, which this fixture's own sum would blow
    /// past.
    /// `system_program` is a real account on the cluster (21 bytes of data, owned by the
    /// native loader) — the loader charges `64 + len` for it like any other loaded key, so its length is
    /// read from the same snapshot, never assumed to be 0.
    #[test]
    fn system_program_data_length_is_read_from_the_snapshot_not_assumed_zero() {
        let mut f = Fixture::happy_path();
        f.fetch
            .accounts
            .insert(solana_system_interface::program::id(), vec![0xABu8; 21]);
        let a = f.anchor().expect("should anchor cleanly");
        assert_eq!(f.fetch.calls, 1, "still exactly one read per anchor()");
        // post_root_proved_ix order: authority, root, pending, pred, registry, inbox batch, chain_config,
        // global_config, treasury, system_program, then the program and its ProgramData.
        assert_eq!(a.account_data_lens.len(), 12);
        assert_eq!(
            a.account_data_lens[9], 21,
            "system_program's live data length must come from the snapshot"
        );
    }

    #[test]
    fn the_derived_loaded_accounts_limit_from_live_lengths_is_17_pages_not_the_old_8() {
        // authority, root (MIN_LEN), candidate pending (new), predecessor pending (PENDING_LEN),
        // registry (REGISTRY_LEN_V2), inbox batch (account_len(1)), chain_config (LEN_V2),
        // global_config (LEN), treasury, system_program, program (36), ProgramData (543,600).
        let lens = [
            0,
            rome_zk_layouts::root::MIN_LEN,
            0,
            rome_zk_layouts::pending::PENDING_LEN,
            rome_zk_layouts::registry::REGISTRY_LEN_V2,
            rome_zk_layouts::batch::account_len(1),
            rome_zk_layouts::chain_config::LEN_V2,
            rome_zk_layouts::global_config::LEN,
            0,
            0,
            36,
            543_600,
        ];
        let derived = crate::poster::preflight_loaded_accounts_data_size_limit(&lens);
        assert!(
            derived >= 557_056,
            "derived {derived} must be >= 557,056 (17 pages) — the old 262,144-byte (8-page) default \
             cannot load this program's real ProgramData"
        );
        assert_eq!(derived, 17 * 32 * 1024);
        assert!(
            derived > 262_144,
            "the old 256 KiB literal must not survive contact with the real ProgramData length"
        );
    }

    /// **Mutation: drop ProgramData from the lens -> this test goes red** (the whole point of
    /// fetching it live rather than trusting a literal).
    #[test]
    fn dropping_programdata_from_the_lens_collapses_the_derived_limit() {
        let lens_without_programdata = [
            0,
            rome_zk_layouts::root::MIN_LEN,
            0,
            rome_zk_layouts::pending::PENDING_LEN,
            rome_zk_layouts::registry::REGISTRY_LEN_V2,
            rome_zk_layouts::batch::account_len(1),
            rome_zk_layouts::chain_config::LEN_V2,
            rome_zk_layouts::global_config::LEN,
            0,
            0,
            36,
            // ProgramData dropped.
        ];
        let derived =
            crate::poster::preflight_loaded_accounts_data_size_limit(&lens_without_programdata);
        assert!(
            derived < 557_056,
            "dropping ProgramData must collapse the derived limit well below the real requirement, got {derived}"
        );
    }
}
