//! Decision logic the operator tools share, kept in the library so the `rome-zk-ops` binary and the examples
//! use one copy and the tests cover it directly: the permissionless registration nonce and its recorded
//! pair, and the `pending_mask` of an exit-config proposal. Nothing here reads a key or calls an RPC.

use solana_program::pubkey::Pubkey;

/// What looking up the authority's `perm_nonce` account came back with: `Ok(None)` is a missing account
/// (nonce 0), `Ok(Some(data))` is the account data, `Err` is an RPC failure of any kind.
pub type NonceLookup = Result<Option<Vec<u8>>, String>;

/// A missing account is nonce 0; any RPC failure, or an account that does not decode, is a named refusal
/// (`NonceLookupFailed`) and never nonce 0: an unreachable or rate-limited RPC must not read as "no
/// registrations yet".
pub fn perm_nonce_from_lookup(lookup: NonceLookup) -> Result<u64, String> {
    match lookup {
        Ok(None) => Ok(0),
        Ok(Some(data)) => rome_zk_layouts::perm_nonce::read(&data)
            .map(|f| f.nonce)
            .map_err(|e| {
                format!("NonceLookupFailed: the perm_nonce account does not decode: {e:?}")
            }),
        Err(e) => Err(format!(
            "NonceLookupFailed: the RPC request failed, so the registration nonce could not be read (this is not a missing account); the client said: {e}"
        )),
    }
}

/// The nonce and chain id a permissionless registration sends when the caller recorded them with
/// `--nonce <n>` and `--expect-chain-id <id>`: the program checks `nonce >= current` and
/// `chain_id == derive(authority, nonce)` in the same transaction, so a stale id is refused there and no
/// deposit is taken. `Ok(None)` means neither flag was given: read the nonce from Solana and derive the id
/// (the behaviour before the flags existed). `--expect-chain-id` needs `--nonce`, because the program
/// cannot check an id without the nonce it was derived from. Refusals are named and exit 2.
pub fn plan_permissionless(
    reserved: bool,
    authority: &Pubkey,
    nonce_flag: Option<&str>,
    expect_flag: Option<&str>,
) -> Result<Option<(u64, u64)>, String> {
    if nonce_flag.is_none() && expect_flag.is_none() {
        return Ok(None);
    }
    if reserved {
        return Err(
            "PermissionlessFlagsOnReserved: --nonce and --expect-chain-id belong to the \
             permissionless path; --reserved takes --chain-id"
                .to_string(),
        );
    }
    let nonce_flag = match (nonce_flag, expect_flag) {
        (Some(n), _) => n,
        (None, _) => {
            return Err(
                "ExpectChainIdNeedsNonce: --expect-chain-id <id> also needs --nonce <n>, the \
                 nonce the id was derived from, so the program can check the pair"
                    .to_string(),
            )
        }
    };
    let nonce: u64 = nonce_flag
        .parse()
        .map_err(|_| format!("BadNonce: --nonce {nonce_flag:?} is not an unsigned number"))?;
    let derived = crate::derive_permissionless_chain_id(authority, nonce);
    if let Some(e) = expect_flag {
        let expected: u64 = e.parse().map_err(|_| {
            format!("BadExpectChainId: --expect-chain-id {e:?} is not an unsigned number")
        })?;
        if expected != derived {
            return Err(format!(
                "ExpectedChainIdMismatch: --expect-chain-id {expected} is not the id this authority \
                 gets at nonce {nonce} ({derived}); nothing was sent"
            ));
        }
    }
    Ok(Some((nonce, derived)))
}

/// The `pending_mask` a `ProposeExitConfig` with these four optional fields will write — the same bits
/// `programs/zk-settlement`'s `governance::propose_exit_config` computes, so `propose-exit-config` can
/// print the resulting mask without a round trip to read it back.
pub fn pending_mask(
    exit_portal: Option<[u8; 20]>,
    bridge_program: Option<Pubkey>,
    exit_cap: Option<u64>,
    poster_bond: Option<u64>,
) -> u8 {
    use rome_zk_layouts::exit::exit_config::{
        PENDING_MASK_BOND, PENDING_MASK_BRIDGE, PENDING_MASK_CAP, PENDING_MASK_PORTAL,
    };
    let mut mask = 0u8;
    if exit_portal.is_some() {
        mask |= PENDING_MASK_PORTAL;
    }
    if bridge_program.is_some() {
        mask |= PENDING_MASK_BRIDGE;
    }
    if exit_cap.is_some() {
        mask |= PENDING_MASK_CAP;
    }
    if poster_bond.is_some() {
        mask |= PENDING_MASK_BOND;
    }
    mask
}

/// Which `pending_mask` bits are set, by name (empty mask -> "none") — `show-exit-config`'s own
/// human-readable rendering of the bitmask `rome_zk_layouts::exit::exit_config` defines.
pub fn pending_mask_names(mask: u8) -> String {
    use rome_zk_layouts::exit::exit_config::{
        PENDING_MASK_BOND, PENDING_MASK_BRIDGE, PENDING_MASK_CAP, PENDING_MASK_PORTAL,
    };
    if mask == 0 {
        return "none".to_string();
    }
    let mut names = Vec::new();
    if mask & PENDING_MASK_PORTAL != 0 {
        names.push("portal");
    }
    if mask & PENDING_MASK_BRIDGE != 0 {
        names.push("bridge");
    }
    if mask & PENDING_MASK_CAP != 0 {
        names.push("cap");
    }
    if mask & PENDING_MASK_BOND != 0 {
        names.push("bond");
    }
    names.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey::Pubkey;

    fn authority() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn nonce_account(nonce: u64) -> Vec<u8> {
        let mut d = vec![0u8; rome_zk_layouts::perm_nonce::LEN];
        rome_zk_layouts::perm_nonce::write(
            &mut d,
            &rome_zk_layouts::perm_nonce::NonceFields {
                authority: authority().to_bytes(),
                nonce,
            },
        );
        d
    }

    #[test]
    fn missing_nonce_account_is_nonce_zero() {
        assert_eq!(perm_nonce_from_lookup(Ok(None)), Ok(0));
    }

    #[test]
    fn existing_nonce_account_gives_its_nonce() {
        assert_eq!(perm_nonce_from_lookup(Ok(Some(nonce_account(3)))), Ok(3));
    }

    #[test]
    fn rpc_error_is_a_named_refusal_not_nonce_zero() {
        let err = perm_nonce_from_lookup(Err("429 Too Many Requests".into())).unwrap_err();
        assert!(err.starts_with("NonceLookupFailed"), "{err}");
        assert!(err.contains("429"), "{err}");
    }

    #[test]
    fn rpc_error_says_the_rpc_could_not_be_read_before_the_clients_text() {
        // solana-rpc-client words every send error "AccountNotFound: pubkey=...": that text must not be read as a
        // missing account, so our own sentence comes first and says so.
        let err =
            perm_nonce_from_lookup(Err("AccountNotFound: pubkey=Abc: connection refused".into()))
                .unwrap_err();
        let ours = err.find("the RPC request failed").expect(&err);
        let theirs = err.find("AccountNotFound").expect(&err);
        assert!(ours < theirs, "{err}");
        assert!(err.contains("not a missing account"), "{err}");
        assert!(err.contains("connection refused"), "{err}");
    }

    #[test]
    fn undecodable_nonce_account_is_a_named_refusal() {
        let err = perm_nonce_from_lookup(Ok(Some(vec![1, 2, 3]))).unwrap_err();
        assert!(err.starts_with("NonceLookupFailed"), "{err}");
    }

    #[test]
    fn no_flags_means_read_the_nonce_as_before() {
        assert_eq!(
            plan_permissionless(false, &authority(), None, None),
            Ok(None)
        );
    }

    #[test]
    fn recorded_nonce_and_id_are_sent_as_given() {
        let id = crate::derive_permissionless_chain_id(&authority(), 4);
        let plan = plan_permissionless(false, &authority(), Some("4"), Some(&id.to_string()));
        assert_eq!(plan, Ok(Some((4, id))));
    }

    #[test]
    fn nonce_alone_derives_the_id_from_it() {
        let id = crate::derive_permissionless_chain_id(&authority(), 2);
        assert_eq!(
            plan_permissionless(false, &authority(), Some("2"), None),
            Ok(Some((2, id)))
        );
    }

    #[test]
    fn expect_chain_id_without_nonce_is_refused_by_name() {
        let err = plan_permissionless(false, &authority(), None, Some("5")).unwrap_err();
        assert!(err.starts_with("ExpectChainIdNeedsNonce"), "{err}");
    }

    #[test]
    fn expected_id_that_does_not_match_the_nonce_is_refused_by_name() {
        let err = plan_permissionless(false, &authority(), Some("1"), Some("5")).unwrap_err();
        assert!(err.starts_with("ExpectedChainIdMismatch"), "{err}");
    }

    #[test]
    fn unparsable_flag_values_are_refused_by_name() {
        let e1 = plan_permissionless(false, &authority(), Some("x"), None).unwrap_err();
        assert!(e1.starts_with("BadNonce"), "{e1}");
        let e2 = plan_permissionless(false, &authority(), Some("1"), Some("y")).unwrap_err();
        assert!(e2.starts_with("BadExpectChainId"), "{e2}");
    }

    #[test]
    fn the_flags_are_refused_on_the_reserved_path() {
        let err = plan_permissionless(true, &authority(), Some("1"), None).unwrap_err();
        assert!(err.starts_with("PermissionlessFlagsOnReserved"), "{err}");
        assert_eq!(
            plan_permissionless(true, &authority(), None, None),
            Ok(None)
        );
    }
}
