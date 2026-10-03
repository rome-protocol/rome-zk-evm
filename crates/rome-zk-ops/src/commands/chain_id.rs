//! `chain-id`: the chain id the next permissionless registration of an authority will get. Reads, sends nothing.

use crate::chain::Chain;
use crate::error::OpsError;
use solana_program::pubkey::Pubkey;
use zk_settlement_client::ops_plan::perm_nonce_from_lookup;
use zk_settlement_client::PermissionlessChainId;

/// The authority's `perm_nonce`: 0 when its account does not exist yet, and `NonceLookupFailed` (exit 1) when the
/// RPC cannot be asked, so an unreachable or rate-limited RPC never reads as "no registrations yet".
pub async fn read_perm_nonce<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    authority: &Pubkey,
) -> Result<u64, OpsError> {
    let (nonce_pda, _) = zk_settlement_client::perm_nonce_pda(settlement, authority);
    perm_nonce_from_lookup(chain.account(&nonce_pda).await).map_err(refusal)
}

/// `ops_plan` words its refusals as "Name: detail"; split them back into the named error.
pub(crate) fn refusal(text: String) -> OpsError {
    let (name, detail) = text.split_once(": ").unwrap_or(("Refused", &text));
    let name: &'static str = match name {
        "NonceLookupFailed" => "NonceLookupFailed",
        "PermissionlessFlagsOnReserved" => "PermissionlessFlagsOnReserved",
        "ExpectChainIdNeedsNonce" => "ExpectChainIdNeedsNonce",
        "BadNonce" => "BadNonce",
        "BadExpectChainId" => "BadExpectChainId",
        "ExpectedChainIdMismatch" => "ExpectedChainIdMismatch",
        _ => "Refused",
    };
    let exit = if name == "NonceLookupFailed" { 1 } else { 2 };
    OpsError {
        name,
        detail: detail.to_string(),
        exit_code: exit,
    }
}

pub async fn run<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    authority: Pubkey,
) -> Result<PermissionlessChainId, OpsError> {
    let nonce = read_perm_nonce(chain, settlement, &authority).await?;
    Ok(PermissionlessChainId::new(authority, nonce))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    fn auth() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    #[tokio::test]
    async fn a_missing_nonce_account_is_nonce_zero_and_nothing_is_sent() {
        let chain = FakeChain::default();
        let id = run(&chain, &program(), auth()).await.unwrap();
        assert_eq!(id.nonce, 0);
        assert_eq!(
            id.chain_id,
            zk_settlement_client::derive_permissionless_chain_id(&auth(), 0)
        );
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn an_rpc_failure_is_nonce_lookup_failed_never_nonce_zero() {
        let (pda, _) = zk_settlement_client::perm_nonce_pda(&program(), &auth());
        let chain = FakeChain::default().failing(pda, "429 Too Many Requests");
        let err = run(&chain, &program(), auth()).await.unwrap_err();
        assert_eq!(err.name, "NonceLookupFailed");
        assert_eq!(err.exit_code, 1);
        assert!(err.detail.contains("429"), "{err}");
    }

    #[tokio::test]
    async fn the_output_is_three_key_value_lines() {
        let chain = FakeChain::default();
        let id = run(&chain, &program(), auth()).await.unwrap();
        let text = id.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("authority="));
        assert!(lines[1].starts_with("nonce="));
        assert!(lines[2].starts_with("chain_id="));
    }
}
