//! Reads the chain's drift bound off `chain_config` (derive reads `max_drift_secs` from `chain_config`
//! at start, so derive, guest and program enforce one number).
//! Until this module, [`crate::config::Config::max_drift_secs`] was the *only* source
//! [`crate::pipeline::DerivePipeline::with_drift_bound`] ever saw — a chain's actual on-chain
//! `chain_config.max_drift_secs` was never consulted, so a TOML value that silently
//! drifted from the chain's own (or the guest's, once it enforces the bound) would go undetected. [`chain_drift_bound`]
//! is the read; [`crate::config::reconcile_drift_bound`] is the pure function that decides what a
//! TOML override means against it.
//!
//! Same shape as [`crate::resume::resume_anchor`] (module doc there): one [`crate::reader::AccountReader`]
//! read at FINALIZED commitment, decoded through the one shared decoder
//! (`zk_settlement_client::decode_chain_config_account`), never re-derived by hand. Every refusal names
//! the PDA, the chain id and the reason — never a bare "critical".

use solana_program::pubkey::Pubkey;

use crate::reader::AccountReader;
use crate::PipelineError;

/// Reads `chain_config_pda(settlement_program_id, chain_id)` at FINALIZED (via `reader`, the same
/// [`AccountReader`] [`crate::resume::resume_anchor`] uses) and returns the chain's committed
/// `max_drift_secs`. Refuses by name, [`PipelineError::Critical`], on every shape that is not "a v2
/// account naming this chain with a nonzero bound":
///
/// - **absent** — the chain is not registered under this settlement program at all;
/// - **undecodable** — `decode_chain_config_account` itself failed (bad magic/version/length);
/// - **chain_id mismatch** — the account at this chain's own PDA reports a different chain id (can only
///   mean this crate's PDA derivation drifted from `zk-settlement-client`'s, mirroring
///   `crate::resume::resume_anchor`'s equivalent check);
/// - **v1** (`max_drift_secs: None`) — `MigrateChainV2` has not run for this chain yet;
/// - **`Some(0)`** — unconstructable on chain (`InitChainV2`/`MigrateChainV2`/`SetDriftBound` all refuse
///   `0` with `DriftBoundZero`), refused here too as defense in depth (like the negative
///   `open_unix_ts` precedent: refuse a value the source should never have produced, rather than trust
///   that it never will).
///
/// An RPC-layer failure (never "the account doesn't exist" — that is `Ok(None)`, handled above) surfaces
/// as whatever [`AccountReader::get_account_data`] itself returns, always [`PipelineError::Temporary`]
/// for [`crate::reader::RpcAccountReader`] — this node must keep retrying a transient RPC hiccup, not
/// treat it as a strict-validity fault.
pub async fn chain_drift_bound<R: AccountReader>(
    reader: &mut R,
    settlement_program_id: &Pubkey,
    chain_id: u64,
) -> Result<u64, PipelineError> {
    let (chain_config_pda, _) =
        zk_settlement_client::chain_config_pda(settlement_program_id, chain_id);
    // A lamports-only account at the PDA (anyone can pre-fund a PDA before the chain is registered)
    // reads as `Some(vec![])`, not `None` — absence of a chain_config either way.
    let data = match reader.get_account_data(chain_config_pda).await? {
        Some(d) if !d.is_empty() => d,
        _ => {
            return Err(PipelineError::Critical(format!(
                "chain_config missing at {chain_config_pda} for chain {chain_id} under settlement program \
                 {settlement_program_id} — the chain is not registered under this settlement program"
            )));
        }
    };
    let cfg = zk_settlement_client::decode_chain_config_account(&data).map_err(|e| {
        PipelineError::Critical(format!(
            "chain_config at {chain_config_pda} for chain {chain_id}: undecodable: {e}"
        ))
    })?;
    if cfg.chain_id != chain_id {
        return Err(PipelineError::Critical(format!(
            "chain_config at the expected PDA for chain {chain_id} reports chain {} instead",
            cfg.chain_id
        )));
    }
    match cfg.max_drift_secs {
        None => Err(PipelineError::Critical(format!(
            "chain_config at {chain_config_pda} for chain {chain_id} is v1 (no max_drift_secs on \
             record) — run MigrateChainV2 first"
        ))),
        Some(0) => Err(PipelineError::Critical(format!(
            "chain_config at {chain_config_pda} for chain {chain_id} carries max_drift_secs = 0 — \
             unconstructable on chain (InitChainV2/MigrateChainV2/SetDriftBound all refuse it), refusing \
             here too"
        ))),
        Some(v) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeAccountReader;
    use rome_zk_layouts::chain_config::{ChainConfigFields, LEN_V1, LEN_V2};

    const CHAIN_ID: u64 = 200_101;

    fn v2_bytes(chain_id: u64, max_drift_secs: u64) -> Vec<u8> {
        let mut d = vec![0u8; LEN_V2];
        rome_zk_layouts::chain_config::write(
            &mut d,
            &ChainConfigFields {
                chain_id,
                reserved: true,
                deposit_lamports: 0,
                deposit_refunded: true,
                registered_slot: 1,
                posted_batches: 0,
                fee_base_lamports: 0,
                fee_bps: 0,
                max_drift_secs: Some(max_drift_secs),
            },
        );
        d
    }

    fn v1_bytes(chain_id: u64) -> Vec<u8> {
        let mut d = vec![0u8; LEN_V1];
        rome_zk_layouts::chain_config::write(
            &mut d,
            &ChainConfigFields {
                chain_id,
                reserved: true,
                deposit_lamports: 0,
                deposit_refunded: true,
                registered_slot: 1,
                posted_batches: 0,
                fee_base_lamports: 0,
                fee_bps: 0,
                max_drift_secs: None,
            },
        );
        d
    }

    /// (a): a v2 `chain_config` naming a bound other than any hardcoded default (45, not 60) — the read
    /// must carry that exact chain value through, not some stand-in constant.
    #[tokio::test]
    async fn a_v2_chain_config_returns_its_own_max_drift_secs() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        reader.accounts.insert(pda, v2_bytes(CHAIN_ID, 45));
        let bound = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap();
        assert_eq!(bound, 45);
    }

    /// A lamports-only account at the PDA (anyone can pre-fund a PDA before the chain
    /// is registered) has `Some(vec![])` data, not `None` — it must still be refused as NOT REGISTERED,
    /// never as a layout problem an operator would chase.
    #[tokio::test]
    async fn an_empty_data_account_at_the_pda_is_refused_as_not_registered() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        reader.accounts.insert(pda, Vec::new());
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not registered"),
            "wrong refusal reason: {msg}"
        );
        assert!(
            !msg.contains("undecodable"),
            "an empty account is absence, not corruption: {msg}"
        );
    }

    /// The undecodable arm pinned: bytes with the wrong magic are refused naming the decode error.
    #[tokio::test]
    async fn bytes_with_a_bad_magic_are_refused_as_undecodable() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        let mut d = v2_bytes(CHAIN_ID, 45);
        d[0] ^= 0xff;
        reader.accounts.insert(pda, d);
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("undecodable"), "{err}");
    }

    /// (b): a v1 account (pre-`MigrateChainV2`) is refused by name, naming the required migration.
    #[tokio::test]
    async fn a_v1_chain_config_is_refused_naming_migrate_chain_v2() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        reader.accounts.insert(pda, v1_bytes(CHAIN_ID));
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        match err {
            PipelineError::Critical(msg) => assert!(
                msg.contains("MigrateChainV2"),
                "expected the required migration named in the refusal, got {msg:?}"
            ),
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// (c): no `chain_config` account at all is refused by name, naming registration — never a silent
    /// fallback to any default bound.
    #[tokio::test]
    async fn an_absent_chain_config_is_refused_naming_registration() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        match err {
            PipelineError::Critical(msg) => assert!(
                msg.contains("not registered"),
                "expected the missing-registration reason named in the refusal, got {msg:?}"
            ),
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    /// (d): `Some(0)` is unconstructable on chain (the program itself refuses `DriftBoundZero`) but
    /// derive refuses it too, defense in depth — same shape as the negative `open_unix_ts` check.
    #[tokio::test]
    async fn a_zero_max_drift_secs_is_refused_even_though_the_program_cannot_produce_it() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        reader.accounts.insert(pda, v2_bytes(CHAIN_ID, 0));
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }

    /// A decoded account reporting a different chain id than the one this PDA was derived for can only
    /// mean this crate's PDA derivation drifted from `zk-settlement-client`'s (mirrors
    /// `crate::resume::resume_anchor`'s equivalent check) — fail loudly, never silently trust it.
    #[tokio::test]
    async fn a_chain_id_mismatch_is_critical() {
        let program_id = Pubkey::new_unique();
        let mut reader = FakeAccountReader::default();
        let (pda, _) = zk_settlement_client::chain_config_pda(&program_id, CHAIN_ID);
        reader.accounts.insert(pda, v2_bytes(CHAIN_ID + 1, 45));
        let err = chain_drift_bound(&mut reader, &program_id, CHAIN_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, PipelineError::Critical(_)));
    }
}
