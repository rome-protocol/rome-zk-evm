//! The exit-prover's own attempt loop: given one decoded `ExitInitiated` message,
//! [`attempt_exit`] reads the newest Final batch, fetches its `eth_getProof`, verifies it LOCALLY against
//! that batch's `state_root` (a doomed proof must never spend a fee — this crate's mirror
//! of `programs/zk-settlement::exit::prove_exit`'s own checks), and only then builds and sends
//! `ProveExit`. Every named refusal is returned BEFORE [`rome_zk_solana_sender::Sender`] is
//! ever touched; the four ways a SEND can come back are then classified by the on-chain error it carries
//! (mirrors `rome-zk-prover::poster`'s own `settle_error_name` classification pattern, this crate's own
//! smaller table).
//!
//! `Deps`-over-fakes (mirrors `rome-zk-prover::follower`): [`crate::settlement::SettlementReader`],
//! [`crate::rpc::VerifierRpc`] and [`rome_zk_solana_sender::Sender`] are all traits; this module's own
//! tests (and `tests/attempt_exit.rs`) drive every refusal and every send classification against small,
//! scripted fakes — never a live cluster or verifier node.

use rome_zk_layouts::exit::{bit_is_set, nullifier_page, ExitMessage};
use rome_zk_solana_sender::{sender_error_transaction_error, SendTuning, Sender};
use solana_hash::Hash;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_program::pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction_error::TransactionError;
use zk_settlement_client::ExitMessageArg;

use crate::proof::{exit_proof_from_get_proof, verify_exit_inclusion, VerifyRefusal};
use crate::rpc::VerifierRpc;
use crate::settlement::{ReadError, SettlementReader};

/// `programs/zk-settlement::errors::SettleError`'s own `#[repr(u32)]` discriminants this crate classifies
/// a failed send by, pinned by name (`errors_pin_test`, this module), never re-derived, since
/// this crate does not depend on the program crate itself.
pub const ERR_EXIT_ALREADY_PROVED: u32 = 65;
pub const ERR_EXIT_CAP_EXCEEDED: u32 = 68;

/// Every way a call refuses BEFORE the `Sender` is ever touched — a doomed proof never spends
/// a fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The verifier lacks state at the Final batch's own last block (its own `--rpc.eth-proof-window`)
    /// — the message persists in L2 storage; a later poll against a (possibly later) Final
    /// root retries this same message.
    StateUnavailableAtRoot { number: u64 },
    /// The local MPT re-verify (mirrors the on-chain check) refused — see [`VerifyRefusal`].
    Verify(VerifyRefusal),
    /// The projected signed V1 `ProveExit` transaction exceeds the configured byte budget — refused
    /// before any send, never truncated or staged (a staged path is a LATER contingency, not
    /// built here).
    ProofTooLarge { len: usize, max: usize },
    /// `exit_config.exit_portal` is unset (all-zero) — exits are not configured for this chain yet.
    ExitConfigUnset,
    /// `root.exit_cap_per_window == 0` — mirrors the on-chain `ExitCapUnset` (error 64):
    /// `0` disables exits fail-closed. Governance can activate `exit_portal` alone (an independent
    /// `pending_mask` bit from the cap), so `(exit_portal set, exit_cap_per_window == 0)` is reachable;
    /// refused here BEFORE any fetch/verify/send so that window never produces a doomed `ProveExit`.
    ExitCapUnset,
    /// `root.challenge_window_slots == 0` — cannot compute a window index.
    ChallengeWindowZero,
    /// `root.head_final_batch == 0` — nothing is Final yet; nothing to prove against.
    NoFinalBatch,
    /// This message's own `amount`, converted to [`rome_zk_layouts::exit::cap_units`], exceeds
    /// `root.exit_cap_per_window` all by itself — no window (empty or not) could ever admit it, so this is
    /// refused independent of any `exit_window` read. Distinct from
    /// [`Outcome::QueuedForWindow`]: that is "try the next window"; this is "no window will ever work
    /// under the CURRENT cap" — still re-tried (at zero fee) if governance later raises the cap.
    ExceedsWindowCap { units: u64, cap: u64 },
    /// `message.asset != [0; 20]` — v1 is native-asset only; the on-chain `prove_exit` refuses
    /// `UnsupportedAsset` before any MPT work, and this is its local mirror: such
    /// a message would otherwise VERIFY locally (the portal really did log that hash), be sent, and burn
    /// `max_send_attempts` fees on an error that is neither 65 nor 68.
    UnsupportedAsset,
}

/// Every outcome [`attempt_exit`] can return, refusal and send classification both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The `ProveExit` transaction was sent and confirmed — carrying the window it landed in and the cap
    /// units it spent there, so the follower can account its OWN sends against that window before the
    /// `finalized` reader can see them (see `Follower::sent_units`).
    Sent { window_index: u64, units: u64 },
    /// The on-chain send failed `ExitAlreadyProved` (65) — this exit is already done; not a failure,
    /// advance (the nullifier bit is the source of truth, never re-sent).
    AlreadyProved,
    /// The on-chain send failed `ExitCapExceeded` (68) — re-queue to the next window and retry there,
    /// never a halt.
    QueuedForWindow { next_index: u64, retry_slot: u64 },
    /// A named local refusal — the `Sender` was never called.
    Refused(Refusal),
    /// The send failed for any other on-chain/RPC reason — the caller's own retry/halt policy decides
    /// what to do with the message text.
    SendFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("reading settlement state: {0}")]
    Read(#[from] ReadError),
    #[error("verifier RPC: {0}")]
    Verifier(#[from] crate::rpc::VerifierError),
    #[error("building the ProveExit transaction: {0}")]
    Build(String),
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn message_arg(m: &ExitMessage) -> ExitMessageArg {
    ExitMessageArg {
        nonce: m.nonce,
        l2_sender: m.l2_sender,
        sol_recipient: m.sol_recipient,
        asset: m.asset,
        amount: m.amount,
    }
}

/// Extracts the on-chain custom error code a failed send carries, via
/// [`rome_zk_solana_sender::sender_error_transaction_error`] (never a second, hand-rolled unwrap of
/// `SenderError`'s variants).
fn custom_error_code(e: &rome_zk_solana_sender::SenderError) -> Option<u32> {
    match sender_error_transaction_error(e) {
        Some(TransactionError::InstructionError(_, InstructionError::Custom(c))) => Some(c),
        _ => None,
    }
}

/// Builds a `ProveExit` instruction with a THROWAWAY signer (never the real payer) purely to measure the
/// signed V1 transaction's serialized byte length — valid because every `Pubkey` is exactly 32 bytes and
/// every `Signature` exactly 64 regardless of its value, so this measurement is byte-for-byte identical
/// to what the REAL payer's own signed transaction would serialize to. This is also why
/// [`attempt_exit`]'s pre-send size guard never needs (and never receives) real key material.
#[allow(clippy::too_many_arguments)]
pub fn measure_prove_exit_tx_len(
    program_id: &Pubkey,
    chain_id: u64,
    batch: u64,
    message: ExitMessageArg,
    proof: rome_zk_mpt::ExitProof,
    window_index: u64,
    page: u64,
    tuning: SendTuning,
) -> Result<usize, CoreError> {
    let shadow = Keypair::new();
    let shadow_payer = rome_zk_solana_sender::compat::from_v1_pubkey(&shadow.pubkey());
    let ix = zk_settlement_client::prove_exit_ix(
        program_id,
        &shadow_payer,
        chain_id,
        batch,
        message,
        proof,
        window_index,
        page,
    );
    let tx = rome_zk_solana_sender::build_v1_tx(
        &shadow,
        std::slice::from_ref(&ix),
        tuning.compute_unit_limit,
        tuning.loaded_accounts_data_size_limit,
        tuning.priority_fee_micro_lamports,
        Hash::default(),
    )
    .map_err(|e| CoreError::Build(e.to_string()))?;
    Ok(wincode::serialize(&tx)
        .map_err(|e| CoreError::Build(e.to_string()))?
        .len())
}

/// Everything one attempt needs beyond the message itself and the traits it reads through.
#[derive(Debug, Clone, Copy)]
pub struct AttemptParams {
    pub program_id: Pubkey,
    /// The REAL production payer's pubkey — must equal whatever the wired `Sender` actually signs with
    /// (the same "poster key == sender's own key" contract `rome-zk-prover::poster`'s `PostParams`
    /// carries). Never used for the pre-send size measurement (see [`measure_prove_exit_tx_len`]).
    pub payer: Pubkey,
    pub now_slot: u64,
    pub max_tx_bytes: usize,
    pub tuning: SendTuning,
    /// Cap units THIS process has already sent (and seen confirmed) against the window `now_slot` falls
    /// in — `Follower::sent_units_in(window_index)`, passed by `run::poll_once`. The pre-send window check
    /// takes `max(chain spent, this)`: the chain read is at `finalized` and lags our own confirmed sends by the
    /// cluster's confirmed-to-finalized gap (0-1 slot on devnet under Alpenglow, about 31 slots on a TowerBFT cluster).
    /// `0` when the caller has no such accounting.
    pub local_spent_units: u64,
}

/// One attempt to prove `message` against the newest Final batch, from reading settlement state through
/// to a classified send (or a pre-send refusal). Never resends: a caller sees [`Outcome::QueuedForWindow`]
/// or [`Refusal::StateUnavailableAtRoot`] and decides, itself, when to call this again.
pub async fn attempt_exit<R, V, S>(
    settlement: &R,
    verifier: &V,
    sender: &S,
    message: ExitMessage,
    params: &AttemptParams,
) -> Result<Outcome, CoreError>
where
    R: SettlementReader,
    V: VerifierRpc,
    S: Sender,
{
    let root = settlement.read_root()?;
    if root.head_final_batch == 0 {
        return Ok(Outcome::Refused(Refusal::NoFinalBatch));
    }
    if root.challenge_window_slots == 0 {
        return Ok(Outcome::Refused(Refusal::ChallengeWindowZero));
    }
    let exit_config = settlement.read_exit_config()?;
    if exit_config.exit_portal == [0u8; 20] {
        return Ok(Outcome::Refused(Refusal::ExitConfigUnset));
    }
    if root.exit_cap_per_window == 0 {
        return Ok(Outcome::Refused(Refusal::ExitCapUnset));
    }
    // The on-chain `prove_exit`'s own asset gate (v1 native-asset only), mirrored here so a non-native
    // message never reaches a fee-paying send it is doomed to fail.
    if message.asset != [0u8; 20] {
        return Ok(Outcome::Refused(Refusal::UnsupportedAsset));
    }

    // The nullifier bit `ProveExit` sets is the source of truth for whether this
    // exit is already done — read it BEFORE `read_pending`/`eth_getProof`/`Sender` so a restart (which
    // rebuilds its in-process queue from `eth_getLogs` at zero fee cost, `follower::Follower::ingest`)
    // never re-sends an exit it (or a previous run) already proved. An absent page account means no exit
    // on it has ever been proved (bit clear) — never an error. This is ADDED to, not a replacement for,
    // the existing on-chain `ExitAlreadyProved` (65) send classification below, which still catches the
    // race where two followers (or two polls of the same one) both pass this read before either sends.
    let page = nullifier_page(message.nonce);
    if let Some(nullifier_page_account) = settlement.read_nullifier_page(page)? {
        let already_set = bit_is_set(&nullifier_page_account.bits, page, message.nonce)
            .expect("page is derived from nonce via nullifier_page() and cannot mismatch");
        if already_set {
            return Ok(Outcome::AlreadyProved);
        }
    }

    // Cap pre-checks, still before `read_pending`/`eth_getProof`/`Sender` — today
    // `QueuedForWindow` was reached only AFTER an included, fee-paying send (on-chain error 68); these two
    // reads make the common case (an exit that cannot or will not fit the current window) free.
    let challenge_window_slots = root.challenge_window_slots as u64;
    let window_index = params.now_slot / challenge_window_slots;
    // (a) an amount whose own unit count exceeds the WHOLE per-window cap can never clear under the
    // current cap, in any window — an overflow (the amount is too large even to express as a `u64` unit
    // count) is treated the same way, since no real cap could ever admit it either.
    let units = rome_zk_layouts::exit::cap_units(message.amount).unwrap_or(u64::MAX);
    if units > root.exit_cap_per_window {
        return Ok(Outcome::Refused(Refusal::ExceedsWindowCap {
            units,
            cap: root.exit_cap_per_window,
        }));
    }
    // (b) the window this attempt would actually land in may already be spent past its cap by earlier
    // sends this poll never saw land — queue to the next window WITHOUT touching the Sender. The on-chain
    // `ExitCapExceeded` (68) classification below still catches the race where two followers (or two polls
    // of the same one) both pass this read before either sends.
    // The chain read is at `finalized`. When the sender is set to `confirmed`, that read lags this process's OWN sends
    // by the cluster's confirmed-to-finalized gap (0-1 slot on devnet under Alpenglow, about 31 on a TowerBFT cluster);
    // with the default `finalized` a `Sent` is already final and the gap is gone, and the local accounting is a cheap
    // guard that stays. So the follower's own per-window accounting (`params.local_spent_units`) is taken into account
    // too — the max of the two, never the sum (once finalized the chain figure already includes ours). The on-chain 68
    // still catches whatever neither view saw.
    let spent_cap_units = settlement
        .read_exit_window(window_index)?
        .map(|w| w.spent_cap_units)
        .unwrap_or(0)
        .max(params.local_spent_units);
    if spent_cap_units.saturating_add(units) > root.exit_cap_per_window {
        return Ok(Outcome::QueuedForWindow {
            next_index: window_index + 1,
            retry_slot: (window_index + 1) * challenge_window_slots,
        });
    }

    let batch = root.head_final_batch;
    let pending = settlement.read_pending(batch)?;

    let portal_hex = hex0x(&exit_config.exit_portal);
    let slot = message.storage_slot();
    let slot_hex = hex0x(&slot);

    let get_proof = match verifier.eth_get_proof(&portal_hex, &slot_hex, pending.last_block) {
        Ok(r) => r,
        Err(e) if e.is_proof_window_exceeded() => {
            return Ok(Outcome::Refused(Refusal::StateUnavailableAtRoot {
                number: pending.last_block,
            }))
        }
        Err(e) => return Err(CoreError::Verifier(e)),
    };
    let proof = match exit_proof_from_get_proof(&get_proof) {
        Ok(p) => p,
        Err(e) => {
            return Ok(Outcome::Refused(Refusal::Verify(
                VerifyRefusal::ProofInvalid(e),
            )))
        }
    };

    // `challenge_window_slots`/`window_index` were already computed above, before the cap pre-checks —
    // reused here rather than re-derived, since both are fixed for the whole attempt.

    // Bounds/size before the (more expensive) hashing walk — the same cheapest-first order
    // `programs/zk-settlement::exit::prove_exit` itself uses on chain (`ExitProofTooLarge`
    // is checked before `verify_account`/`verify_storage`), and the same order `rome_zk_mpt`'s own
    // `check_bounds` runs internally before any node is ever hashed.
    let tx_len = measure_prove_exit_tx_len(
        &params.program_id,
        root.chain_id,
        batch,
        message_arg(&message),
        proof.clone(),
        window_index,
        page,
        params.tuning,
    )?;
    if tx_len > params.max_tx_bytes {
        return Ok(Outcome::Refused(Refusal::ProofTooLarge {
            len: tx_len,
            max: params.max_tx_bytes,
        }));
    }

    if let Err(refusal) =
        verify_exit_inclusion(&pending.state_root, &exit_config.exit_portal, &slot, &proof)
    {
        return Ok(Outcome::Refused(Refusal::Verify(refusal)));
    }

    let ix = zk_settlement_client::prove_exit_ix(
        &params.program_id,
        &params.payer,
        root.chain_id,
        batch,
        message_arg(&message),
        proof,
        window_index,
        page,
    );

    match sender
        .send_and_confirm(std::slice::from_ref(&ix), params.tuning)
        .await
    {
        Ok(_signature) => Ok(Outcome::Sent {
            window_index,
            units,
        }),
        Err(e) => match custom_error_code(&e) {
            Some(ERR_EXIT_ALREADY_PROVED) => Ok(Outcome::AlreadyProved),
            Some(ERR_EXIT_CAP_EXCEEDED) => Ok(Outcome::QueuedForWindow {
                next_index: window_index + 1,
                retry_slot: (window_index + 1) * challenge_window_slots,
            }),
            _ => Ok(Outcome::SendFailed(e.to_string())),
        },
    }
}

#[cfg(test)]
mod tests {
    //! `errors_pin_test` (pins a mapping the crate reads without depending on
    //! the program crate): `programs/zk-settlement/src/errors.rs` is the source of truth, checked here by
    //! literal against this module's own `ERR_*` constants.
    use super::*;

    #[test]
    fn error_codes_are_pinned_by_name() {
        assert_eq!(ERR_EXIT_ALREADY_PROVED, 65);
        assert_eq!(ERR_EXIT_CAP_EXCEEDED, 68);
    }
}
