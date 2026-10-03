//! What a dry run prints. The transaction is built the way a real send builds it (V1, through the sender's own
//! builder) against an all-zero blockhash and signed with the real keys, so the bytes can be inspected and can
//! never land: the cluster refuses a blockhash it has not seen.

use crate::chain::{COMPUTE_UNIT_LIMIT, LOADED_ACCOUNTS_DATA_SIZE_LIMIT};
use crate::error::OpsError;
use crate::keys::Signers;
use base64::Engine as _;
use solana_program::instruction::Instruction;

/// The V1 transaction for `ixs`, signed against an all-zero blockhash, as base64.
fn signed_base64(ixs: &[Instruction], signers: &Signers) -> Result<String, OpsError> {
    let tx = rome_zk_solana_sender::build_v1_tx(
        &signers.as_tx_signer(),
        ixs,
        COMPUTE_UNIT_LIMIT,
        LOADED_ACCOUNTS_DATA_SIZE_LIMIT,
        0,
        solana_hash::Hash::default(),
    )
    .map_err(|e| OpsError::usage("DryRunBuildFailed", e.to_string()))?;
    let bytes = wincode::serialize(&tx)
        .map_err(|e| OpsError::usage("DryRunBuildFailed", format!("serialize: {e}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn header(label: &str) -> Vec<String> {
    vec![format!(
        "-- dry run: {label} built and signed, nothing sent to any cluster --"
    )]
}

/// A dry run for a transaction of several instructions that are not settlement instructions (the bridge commands).
pub fn dry_run_many(
    label: &str,
    ixs: &[Instruction],
    signers: &Signers,
) -> Result<Vec<String>, OpsError> {
    let mut out = header(label);
    for (i, ix) in ixs.iter().enumerate() {
        let d = ix
            .data
            .first()
            .map_or("none".to_string(), |d| d.to_string());
        out.push(format!(
            "  instruction {}   program {}  discriminant {d}",
            i + 1,
            ix.program_id
        ));
    }
    out.push("  format         V1 transaction (SIMD-0385)".to_string());
    out.push(format!("  tx (base64)    {}", signed_base64(ixs, signers)?));
    Ok(out)
}

pub fn dry_run(label: &str, ix: &Instruction, signers: &Signers) -> Result<Vec<String>, OpsError> {
    let b64 = signed_base64(std::slice::from_ref(ix), signers)?;
    let mut out = header(label);
    out.push(format!("  program        {}", ix.program_id));
    out.push(format!("  discriminant   {}", ix.data[0]));
    if let Ok(decoded) = zk_settlement_client::decode_instruction(&ix.data) {
        out.push(format!("  decoded        {decoded:?}"));
        // The Debug print shows `[u8; 20]` and `Pubkey` fields as raw byte arrays. Show the forms a person
        // typed, so what was typed on the command line is visibly what got encoded.
        if let zk_settlement_client::SettleIx::ProposeExitConfig {
            exit_portal,
            bridge_program,
            ..
        } = &decoded
        {
            if let Some(p) = exit_portal {
                out.push(format!("  exit_portal    0x{}", hex::encode(p)));
            }
            if let Some(b) = bridge_program {
                out.push(format!("  bridge_program {b}"));
            }
        }
    }
    out.push("  format         V1 transaction (SIMD-0385)".to_string());
    out.push(format!("  tx (base64)    {b64}"));
    Ok(out)
}
