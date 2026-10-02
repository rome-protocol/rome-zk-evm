//! Builds a `PostRootProved` send's `PostRootFields` from a decoded guest `PublicValues` and the
//! poster's own chain anchor — `crate::poster::build_post_ix` is the one caller.

use rome_zk_layouts::public_values::PublicValues;
use zk_settlement_client::PostRootFields;

/// The facts only the chain (never the proof) supplies: the batch id being posted, its
/// predecessor, and the predecessor's own state root (the root account's `state_root` when the
/// predecessor is batch 0/genesis, else that predecessor's pending PDA).
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    pub batch: u64,
    pub prev_batch: u64,
    pub pre_state_root: [u8; 32],
}

/// `block_roots_merkle` is a poster claim the proof does not bind — always zero, same
/// as the gate's one-off sender.
pub fn to_post_root_fields(pv: &PublicValues, anchor: Anchor) -> PostRootFields {
    PostRootFields {
        chain_id: pv.chain_id,
        batch: anchor.batch,
        prev_batch: anchor.prev_batch,
        pre_state_root: anchor.pre_state_root,
        first_block: pv.first_number,
        last_block: pv.last_number,
        state_root: pv.state_root,
        block_roots_merkle: [0u8; 32],
        inbox_commitment: pv.inbox_commitment,
        forced_outcome_commitment: pv.forced_outcome_commitment,
        parent_hash: pv.parent_hash,
        last_block_hash: pv.last_block_hash,
        gas_in_batch: pv.gas_used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calldata::from_zisk_proof_file;

    fn gate_proof_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin")
    }

    fn gate_sidecar() -> serde_json::Value {
        let s = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.json"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.json");
        serde_json::from_str(&s).unwrap()
    }

    fn decode_gate_public_values() -> PublicValues {
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        assert_eq!(
            hex::encode(cd.program_vk),
            "e5ea5c144f19aba3e8a72f897dcb565c1b18bc335b54fd93e06b06689c53cb03",
            "the gate proof must decode to the registered vkey of record"
        );
        let pv_bytes =
            rome_zk_layouts::public_values::unpack_zisk_outputs(&cd.publics_512).expect("unpack");
        rome_zk_layouts::public_values::read(&pv_bytes).expect("read PublicValues")
    }

    #[test]
    fn gate_proof_public_values_cover_reset6_batch_1_blocks_1_to_60() {
        let pv = decode_gate_public_values();
        assert_eq!(pv.chain_id, 200101);
        assert_eq!(pv.first_number, 1);
        assert_eq!(pv.last_number, 60);
    }

    #[test]
    fn gate_proof_public_values_match_the_sidecars_own_recorded_expectation() {
        let pv = decode_gate_public_values();
        let sc = gate_sidecar();
        assert_eq!(
            hex::encode(pv.parent_hash),
            sc["parent_hash"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(pv.last_block_hash),
            sc["last_block_hash"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(pv.state_root),
            sc["state_root"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(pv.inbox_commitment),
            sc["inbox_commitment"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(pv.forced_outcome_commitment),
            sc["forced_outcome_commitment"].as_str().unwrap()
        );
        assert_eq!(pv.gas_used, sc["gas_used"].as_u64().unwrap());
    }

    #[test]
    fn to_post_root_fields_builds_the_reset6_batch_1_post_root_args() {
        let pv = decode_gate_public_values();
        let fields = to_post_root_fields(
            &pv,
            Anchor {
                batch: 1,
                prev_batch: 0,
                pre_state_root: [0u8; 32],
            },
        );
        assert_eq!(fields.chain_id, 200101);
        assert_eq!(fields.batch, 1);
        assert_eq!(fields.prev_batch, 0);
        assert_eq!(fields.pre_state_root, [0u8; 32]);
        assert_eq!(fields.first_block, 1);
        assert_eq!(fields.last_block, 60);
        assert_eq!(fields.gas_in_batch, 0);
        assert_eq!(fields.block_roots_merkle, [0u8; 32]);
        assert_eq!(fields.state_root, pv.state_root);
        assert_eq!(fields.parent_hash, pv.parent_hash);
        assert_eq!(fields.last_block_hash, pv.last_block_hash);
        assert_eq!(fields.inbox_commitment, pv.inbox_commitment);
        assert_eq!(
            fields.forced_outcome_commitment,
            pv.forced_outcome_commitment
        );
    }
}
