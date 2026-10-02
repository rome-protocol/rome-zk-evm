//! Converts an `eth_getProof` response ([`crate::rpc::GetProofResult`]) into `rome_zk_mpt::ExitProof` and
//! runs the SAME local verify `ProveExit` (28) itself performs on chain — BEFORE any send
//! (a doomed proof must never spend a fee). This module owns no chain-reading and no
//! sending; [`verify_exit_inclusion`] is a pure function of its inputs, so it is exercised directly
//! against fixtures and hand-built counter-fixtures with no fake needed at all.

use rome_zk_mpt::{Account, ExitProof, MptError, StorageValue};

use crate::rpc::GetProofResult;

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn hex_bytes(s: &str) -> Result<Vec<u8>, hex::FromHexError> {
    let h = strip_0x(s);
    let h = if h.len() % 2 == 1 {
        format!("0{h}")
    } else {
        h.to_string()
    };
    hex::decode(h)
}

fn hex32(s: &str) -> [u8; 32] {
    let h = strip_0x(s);
    let padded = format!("{:0>64}", h);
    let v = hex::decode(padded).expect("eth_getProof 32-byte hex field");
    v.try_into().expect("32 bytes")
}

/// Every proof node (`accountProof`/`storageProof[0].proof`) is a `0x`-prefixed RLP blob; decode the
/// whole list. A node that is not valid hex (a torn/garbled verifier response) refuses with
/// [`MptError::BadRlp`] at its own index — never a panic (a malformed node must error
/// cleanly, not crash the follow loop).
fn nodes_from_hex(hexes: &[String]) -> Result<Vec<Vec<u8>>, MptError> {
    hexes
        .iter()
        .enumerate()
        .map(|(index, h)| hex_bytes(h).map_err(|_| MptError::BadRlp { index }))
        .collect()
}

/// Builds an [`ExitProof`] from a raw `eth_getProof` response — the caller has already checked
/// `storage_proof.len() == 1` and picked the right entry (this crate's `eth_getProof` calls always
/// request exactly one storage key). A malformed proof-node hex string refuses `Err(MptError::BadRlp)`
/// rather than panicking (mirrors [`VerifyRefusal::ProofInvalid`]'s own "malformed node" case — no new
/// error taxonomy introduced).
pub fn exit_proof_from_get_proof(result: &GetProofResult) -> Result<ExitProof, MptError> {
    let account_nodes = nodes_from_hex(&result.account_proof)?;
    let storage_nodes = match result.storage_proof.first() {
        Some(sp) => nodes_from_hex(&sp.proof)?,
        None => Vec::new(),
    };
    Ok(ExitProof {
        account_nodes,
        storage_nodes,
    })
}

/// Named local-verify outcomes: every one refuses BEFORE any send, at zero fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyRefusal {
    /// The account proof's root hash does not equal `pending.state_root` — a proof that was fetched
    /// against a different root than the Final batch this attempt is proving against (the
    /// portal is bound from `exit_config`, but the PROOF must itself hash to the same root the on-chain
    /// `ProveExit` will read).
    RootMismatch,
    /// The account or storage proof failed to verify for any other reason (a malformed node, a path that
    /// diverges from the claimed portal address, a bound blown, etc.) — never sent on chain either, but
    /// distinct from a root disagreement.
    ProofInvalid(MptError),
    /// The storage slot proves ABSENT (or present with a value other than `1`) — the message was never
    /// recorded as sent at this root; mirrors the on-chain `ExitNotSent` refusal.
    ExitNotSent,
}

/// Verifies `proof` against `state_root`/`portal`/`slot`, exactly as `programs/zk-settlement::exit::prove_exit`
/// does on chain (`rome_zk_mpt::verify_account` then `verify_storage`) — the prover's own local mirror of
/// that check, run before the instruction is ever built. Returns the decoded [`Account`] on success (its
/// `storage_root` is not otherwise needed by the caller, but proves the whole path ran).
pub fn verify_exit_inclusion(
    state_root: &[u8; 32],
    portal: &[u8; 20],
    slot: &[u8; 32],
    proof: &ExitProof,
) -> Result<Account, VerifyRefusal> {
    let account = rome_zk_mpt::verify_account(state_root, portal, &proof.account_nodes).map_err(
        |e| match e {
            MptError::RootMismatch => VerifyRefusal::RootMismatch,
            other => VerifyRefusal::ProofInvalid(other),
        },
    )?;
    match rome_zk_mpt::verify_storage(&account.storage_root, slot, &proof.storage_nodes) {
        Ok(StorageValue::Present(v)) => {
            let mut expected = [0u8; 32];
            expected[31] = 1;
            if v == expected {
                Ok(account)
            } else {
                Err(VerifyRefusal::ExitNotSent)
            }
        }
        Ok(StorageValue::Absent) => Err(VerifyRefusal::ExitNotSent),
        Err(e) => Err(VerifyRefusal::ProofInvalid(e)),
    }
}

/// `eth_getProof`'s own hex `storage_slot`/`state_root` string -> `[u8; 32]`, left-padded (every fixture
/// and real response is already 32 bytes, but a short hex quantity is padded rather than panicking).
pub fn hex32_field(s: &str) -> [u8; 32] {
    hex32(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const ANVIL_GET_PROOF: &str = include_str!("../../../fixtures/exit/anvil_getProof.json");
    const ANVIL_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root.json");
    const MESSAGE_HASH_JSON: &str = include_str!("../../../fixtures/exit/message_hash.json");
    const ANVIL_GET_PROOF_UNSENT: &str =
        include_str!("../../../fixtures/exit/anvil_getProof_unsent.json");

    #[derive(Deserialize)]
    struct StateRootFixture {
        state_root: String,
    }
    #[derive(Deserialize)]
    struct MessageHashFixture {
        storage_slot: String,
        portal: String,
    }

    fn hex20(s: &str) -> [u8; 20] {
        let v = hex::decode(strip_0x(s)).unwrap();
        v.try_into().unwrap()
    }

    #[test]
    fn recorded_getproof_fixture_decodes() {
        let result: GetProofResult = serde_json::from_str(ANVIL_GET_PROOF).unwrap();
        let root: StateRootFixture = serde_json::from_str(ANVIL_STATE_ROOT).unwrap();
        let msg: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();

        let proof = exit_proof_from_get_proof(&result).unwrap();
        assert_eq!(proof.account_nodes.len(), 3);
        assert_eq!(proof.storage_nodes.len(), 2);

        let state_root = hex32(&root.state_root);
        let portal = hex20(&msg.portal);
        let slot = hex32(&msg.storage_slot);

        let account = verify_exit_inclusion(&state_root, &portal, &slot, &proof)
            .expect("the recorded fixture is a genuine inclusion proof");
        assert_eq!(account.nonce, 1);
    }

    // This module's own half of `local_verify_refuses_on_root_mismatch_before_send`: the pure verify
    // function itself refuses a proof whose account nodes do not hash to the given root.
    // `tests/attempt_exit.rs` covers the "and 0 sends happen" half against a fake Sender.
    #[test]
    fn root_mismatch_is_refused() {
        let result: GetProofResult = serde_json::from_str(ANVIL_GET_PROOF).unwrap();
        let msg: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();
        let proof = exit_proof_from_get_proof(&result).unwrap();
        let portal = hex20(&msg.portal);
        let slot = hex32(&msg.storage_slot);

        // A root that is NOT what this proof's own nodes hash to.
        let wrong_root = [0x42u8; 32];
        let err = verify_exit_inclusion(&wrong_root, &portal, &slot, &proof).unwrap_err();
        assert_eq!(err, VerifyRefusal::RootMismatch);
    }

    #[test]
    fn exclusion_proof_is_refused_as_exit_not_sent() {
        let result: GetProofResult = serde_json::from_str(ANVIL_GET_PROOF_UNSENT).unwrap();
        let root: StateRootFixture = serde_json::from_str(ANVIL_STATE_ROOT).unwrap();
        let msg: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();
        let proof = exit_proof_from_get_proof(&result).unwrap();
        let state_root = hex32(&root.state_root);
        let portal = hex20(&msg.portal);
        // The unsent fixture proves a DIFFERENT (never-sent) slot; use its own key from the response.
        let slot = hex32(&result.storage_proof[0].key);

        let err = verify_exit_inclusion(&state_root, &portal, &slot, &proof).unwrap_err();
        assert_eq!(err, VerifyRefusal::ExitNotSent);
    }

    // A torn/garbled verifier response — a non-hex `accountProof` entry — must error cleanly, never panic
    // the follow loop.
    // Mutation target: revert `hex_bytes`/`nodes_from_hex` to `.expect(...)` — this test then panics
    // (a test-abort "FAILED", not a clean assertion failure) instead of asserting the `Err`.
    #[test]
    fn malformed_getproof_node_errors_not_panics() {
        let result = GetProofResult {
            address: "0x0000000000000000000000000000000000000000".to_string(),
            balance: "0x0".to_string(),
            nonce: "0x0".to_string(),
            storage_hash: "0x0".to_string(),
            code_hash: "0x0".to_string(),
            // "zz" is not valid hex — a torn/garbled node, not a well-formed proof.
            account_proof: vec!["0xzz".to_string()],
            storage_proof: vec![],
        };

        let err = exit_proof_from_get_proof(&result).unwrap_err();
        assert_eq!(err, MptError::BadRlp { index: 0 });
    }
}
