//! Startup preflight: before this process ever sends a fee-spending transaction, prove
//! it is configured against the chain it thinks it is — refuse to run rather than silently misbehave.
//!
//! Two facts, both readable without spending anything:
//! 1. The settlement root account's `authority` must equal this process's own payer —
//!    `open_batch_ix`/`open_chunk_ix` both gate on `payer == root.authority`, so a
//!    mismatch here means every OpenBatch this process attempts will fail on chain; better to say so up
//!    front with a named error than after paying a transaction fee to find out.
//! 2. The settlement registry's `inbox_program` must equal the inbox program id this process is
//!    configured to send chunk instructions to — a mismatch means this process would post DA the
//!    configured settlement chain does not (and never will) recognize as its own inbox.
//!
//! [`check`] is the pure decision (unit-testable without any RPC); [`run`] is the thin async wrapper that
//! fetches the two accounts and decodes them.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// `message` is `describe_rpc_error(&source)`, redacted at construction time —
    /// `source`'s own `Display` can carry a request URL with a secret (an API key in its query string).
    #[error("root account read failed at {root_pda}: {message}")]
    RootAccountRead {
        root_pda: Pubkey,
        message: String,
        #[source]
        source: Box<solana_client::client_error::ClientError>,
    },
    #[error("root account at {root_pda} failed to decode: {source}")]
    RootAccountDecode {
        root_pda: Pubkey,
        #[source]
        source: zk_settlement_client::DecodeError,
    },
    #[error(
        "payer {payer} is not the chain's authority (root account says {root_authority}) — \
         OpenBatch and chunk Open would both reject every transaction this process sends"
    )]
    AuthorityMismatch {
        payer: Pubkey,
        root_authority: Pubkey,
    },
    /// Same redaction as [`Self::RootAccountRead`].
    #[error("registry account read failed at {registry_pda}: {message}")]
    RegistryAccountRead {
        registry_pda: Pubkey,
        message: String,
        #[source]
        source: Box<solana_client::client_error::ClientError>,
    },
    #[error("registry account at {registry_pda} failed to decode: {source}")]
    RegistryAccountDecode {
        registry_pda: Pubkey,
        #[source]
        source: zk_settlement_client::DecodeError,
    },
    #[error(
        "configured inbox program {configured_inbox} does not match the registry's inbox program \
         {registry_inbox} — this process would post DA the settlement chain does not recognize as its own"
    )]
    InboxProgramMismatch {
        configured_inbox: Pubkey,
        registry_inbox: Pubkey,
    },
}

/// Pure check over already-decoded account views — no RPC, no I/O, trivially unit-testable.
pub fn check(
    root: &zk_settlement_client::RootAccount,
    registry: &zk_settlement_client::RegistryAccount,
    payer: &Pubkey,
    configured_inbox_program: &Pubkey,
) -> Result<(), PreflightError> {
    if root.authority != *payer {
        return Err(PreflightError::AuthorityMismatch {
            payer: *payer,
            root_authority: root.authority,
        });
    }
    if registry.inbox_program != *configured_inbox_program {
        return Err(PreflightError::InboxProgramMismatch {
            configured_inbox: *configured_inbox_program,
            registry_inbox: registry.inbox_program,
        });
    }
    Ok(())
}

/// Reads the root and registry accounts under `settlement_program_id` for `chain_id` and runs [`check`]
/// against them. Refuses to run (returns `Err`) rather than let a misconfigured process spend any fee.
pub async fn run(
    rpc: &RpcClient,
    settlement_program_id: &Pubkey,
    chain_id: u64,
    payer: &Pubkey,
    configured_inbox_program: &Pubkey,
) -> Result<(), PreflightError> {
    let (root_pda, _) = zk_settlement_client::root_pda(settlement_program_id, chain_id);
    let root_account =
        rpc.get_account(&root_pda)
            .await
            .map_err(|e| PreflightError::RootAccountRead {
                root_pda,
                message: rome_zk_solana_sender::describe_rpc_error(&e),
                source: Box::new(e),
            })?;
    let root = zk_settlement_client::decode_root_account(&root_account.data).map_err(|e| {
        PreflightError::RootAccountDecode {
            root_pda,
            source: e,
        }
    })?;

    let (registry_pda, _) = zk_settlement_client::registry_pda(settlement_program_id, chain_id);
    let registry_account =
        rpc.get_account(&registry_pda)
            .await
            .map_err(|e| PreflightError::RegistryAccountRead {
                registry_pda,
                message: rome_zk_solana_sender::describe_rpc_error(&e),
                source: Box::new(e),
            })?;
    let registry =
        zk_settlement_client::decode_registry_account(&registry_account.data).map_err(|e| {
            PreflightError::RegistryAccountDecode {
                registry_pda,
                source: e,
            }
        })?;

    check(&root, &registry, payer, configured_inbox_program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with_authority(authority: Pubkey) -> zk_settlement_client::RootAccount {
        zk_settlement_client::RootAccount {
            chain_id: 200_101,
            number: 0,
            parent_hash: [0; 32],
            state_root: [0; 32],
            block_hash: [0; 32],
            updates: 0,
            profile: 0,
            challenge_window_slots: 0,
            prove_window_slots: 0,
            proving_policy: 0,
            poster_bond: 0,
            exit_cap_per_window: 0,
            authority,
            head_pending_batch: 0,
            head_final_batch: 0,
            pending_count: 0,
            max_pending: 0,
        }
    }

    fn registry_with_inbox(inbox_program: Pubkey) -> zk_settlement_client::RegistryAccount {
        zk_settlement_client::RegistryAccount {
            chain_id: 200_101,
            inbox_program,
            count: 1,
            entries: vec![],
        }
    }

    #[test]
    fn passes_when_payer_is_authority_and_inbox_matches() {
        let payer = Pubkey::new_unique();
        let inbox = Pubkey::new_unique();
        let root = root_with_authority(payer);
        let registry = registry_with_inbox(inbox);
        assert!(check(&root, &registry, &payer, &inbox).is_ok());
    }

    #[test]
    fn rejects_when_payer_is_not_the_root_authority() {
        let payer = Pubkey::new_unique();
        let someone_else = Pubkey::new_unique();
        let inbox = Pubkey::new_unique();
        let root = root_with_authority(someone_else);
        let registry = registry_with_inbox(inbox);
        let err = check(&root, &registry, &payer, &inbox).unwrap_err();
        assert!(matches!(err, PreflightError::AuthorityMismatch { .. }));
    }

    #[test]
    fn rejects_when_configured_inbox_does_not_match_the_registry() {
        let payer = Pubkey::new_unique();
        let inbox = Pubkey::new_unique();
        let a_different_inbox = Pubkey::new_unique();
        let root = root_with_authority(payer);
        let registry = registry_with_inbox(a_different_inbox);
        let err = check(&root, &registry, &payer, &inbox).unwrap_err();
        assert!(matches!(err, PreflightError::InboxProgramMismatch { .. }));
    }

    /// Both checks are independent — an authority mismatch is reported even when the inbox program is
    /// also wrong (authority is checked first; a caller fixing one at a time sees the first blocker).
    #[test]
    fn authority_check_runs_before_inbox_check() {
        let payer = Pubkey::new_unique();
        let someone_else = Pubkey::new_unique();
        let inbox = Pubkey::new_unique();
        let a_different_inbox = Pubkey::new_unique();
        let root = root_with_authority(someone_else);
        let registry = registry_with_inbox(a_different_inbox);
        let err = check(&root, &registry, &payer, &inbox).unwrap_err();
        assert!(matches!(err, PreflightError::AuthorityMismatch { .. }));
    }
}
