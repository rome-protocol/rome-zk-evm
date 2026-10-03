//! Builds and sends ONE `PostRootProved`: pre-send guards by name — `ContinuityMismatch`,
//! `LocalVerifyFailed` — checked BEFORE any network call, a loaded-accounts preflight, and
//! post-send refusal classification by the on-chain program's own error name.

use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::anchor::Anchor;
use crate::calldata::RecordChecked;
use crate::config::VkeyOfRecord;
use rome_zk_layouts::public_values::PublicValues;

/// Refused before any send is attempted — zero network calls, zero fee spent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreSendRefusal {
    /// `pv.first_number != predecessor.last_block + 1` — the predecessor's `last_block` comes from the
    /// anchor's own `pred` (the genesis sentinel's `root.number` when `pred` is `None`), never from any
    /// externally-supplied value; this is `settle.rs::validate_post_root`'s own `BadFirstBlock` check,
    /// caught locally before a fee is ever spent.
    #[error(
        "continuity mismatch: proof's first block {got} != predecessor's last_block + 1 ({expected})"
    )]
    ContinuityMismatch { expected: u64, got: u64 },
    /// `veritas::verify_zisk(&abi)` was not `Ok(true)`, or the assembled ABI's own embedded
    /// `program_vk` (bytes `[768..800]`) disagrees with the vkey of record — either way, this proof must
    /// never be sent.
    #[error("local verify failed")]
    LocalVerifyFailed,
    /// The checked proof's own packaged public outputs (`checked.calldata().publics_512`) failed to
    /// decode (`unpack_zisk_outputs`/`read`) — `check_against_record` already confirmed this proof's
    /// `program_vk`/`root_c` match the vkey of record, so this should never fire in practice; refused by
    /// name rather than unwrapped regardless.
    #[error("public values decode failed: {0}")]
    PublicValuesDecodeFailed(String),
    /// The proof's own `chain_id` (from its packaged public values) disagrees with the chain this anchor
    /// snapshot is for (`anchor.root.chain_id` — itself the config's own `chain_id`, since `root_pda` is
    /// derived from it) — a proof built for a different chain must never be posted.
    /// Checked BEFORE any send, no fee spent.
    #[error("chain mismatch: proof chain_id {got} != this chain's {expected}")]
    ChainMismatch { expected: u64, got: u64 },
    /// The key this run signs with is not the chain authority the root account names — the program
    /// would refuse the post by name (`WrongAuthority`) after the fee is spent; refused locally instead,
    /// before any send.
    #[error("signer {got} is not the chain authority {expected}")]
    NotTheAuthority { expected: Pubkey, got: Pubkey },
    /// The proof's own packaged `inbox_commitment` disagrees with the candidate batch's
    /// inbox account's own `acc` (`anchor.inbox_batch_head_plus_1.acc`) — the local mirror of
    /// `settle.rs`'s own `AccMismatch`. A stale artefact (a chain reset, a hand `--once` against another
    /// chain, a resumed cache keyed only on the ELF sha) must never be sent, and never pay the fee to
    /// find out on chain.
    #[error("inbox commitment mismatch: proof committed 0x{got}, this batch's inbox account is 0x{expected}")]
    InboxCommitmentMismatch { expected: String, got: String },
    /// The proof's own packaged `open_unix_ts` disagrees with the candidate batch's
    /// inbox account's own committed `open_unix_ts` — the local mirror of `settle.rs`'s own
    /// `OpenTsMismatch`.
    #[error("open_unix_ts mismatch: proof committed {got}, this batch's inbox account committed {expected}")]
    OpenTsMismatch { expected: i64, got: u64 },
}

/// Decodes the checked proof's own packaged public outputs — the ONLY source of [`PublicValues`] a
/// caller may build a `PostRootProved` from: `PostParams` does not carry a
/// separately-supplied `pv`, so a caller cannot post a proof against public values that disagree with
/// what the proof itself packaged. `checked` is only ever a [`RecordChecked`] (its `program_vk`/`root_c`
/// already match the vkey of record), so this is purely a decode, never a re-verification.
pub fn derive_public_values(checked: &RecordChecked) -> Result<PublicValues, PreSendRefusal> {
    let pv_bytes =
        rome_zk_layouts::public_values::unpack_zisk_outputs(&checked.calldata().publics_512)
            .map_err(|e| PreSendRefusal::PublicValuesDecodeFailed(format!("{e:?}")))?;
    rome_zk_layouts::public_values::read(&pv_bytes)
        .map_err(|e| PreSendRefusal::PublicValuesDecodeFailed(format!("{e:?}")))
}

/// Everything [`build_post_ix`] needs: the chain anchor (already validated by [`crate::anchor::anchor`]),
/// the vkey of record, and the record-checked calldata (already checked against that same vkey by
/// [`crate::calldata::check_against_record`]) — `pv` is derived from `checked` by [`build_post_ix`]
/// itself ([`derive_public_values`]), never supplied separately.
pub struct PostParams<'a> {
    pub anchor: &'a Anchor,
    pub vkey: &'a VkeyOfRecord,
    pub checked: &'a RecordChecked,
    pub settlement_program: &'a Pubkey,
    pub inbox_program: &'a Pubkey,
    pub authority: &'a Pubkey,
    pub treasury: &'a Pubkey,
}

/// The predecessor's `(last_block, state_root)` — the root account's own genesis fields at the sentinel
/// (`anchor.pred.is_none()`), else the predecessor's pending PDA. Mirrors `settle.rs::predecessor_state`
/// exactly (never re-derived differently).
fn predecessor_last_block_and_state_root(anchor: &Anchor) -> (u64, [u8; 32]) {
    match &anchor.pred {
        None => (anchor.root.number, anchor.root.state_root),
        Some(p) => (p.last_block, p.state_root),
    }
}

/// The proof's own ABI-embedded vkey disagrees with the vkey of record, or the assembled ABI fails the
/// real BN254 pairing (`veritas::verify_zisk`) — `check_against_record` already confirmed
/// `program_vk`/`root_c` match the vkey of record, so this is the deeper, cryptographic check that
/// BOTH `build_post_ix` (before any send) AND the follower's resume gate (before trusting
/// a cached artefact) must run — the SAME function, never duplicated logic in two places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("local verify failed")]
pub struct LocalVerifyFailed;

/// Assembles the layout-1 ABI from `checked` and runs the real BN254 pairing on the host
/// (`veritas::verify_zisk`) — the ONE shared pairing check. `check_against_record`
/// must already have run (enforced by the type: only a `RecordChecked` reaches this function), so this is
/// purely the deeper cryptographic layer that a mere `program_vk`/`root_c` match cannot catch (e.g. a
/// proof file whose `proof_bytes` were corrupted after the fact still decodes and still matches the
/// record's vkey words, but no longer satisfies the pairing).
pub fn verify_checked(
    checked: &RecordChecked,
    vkey: &VkeyOfRecord,
) -> Result<(), LocalVerifyFailed> {
    let abi = crate::abi::layout1_from(checked).map_err(|_| LocalVerifyFailed)?;
    let abi_program_vk: [u8; 32] = abi[768..800]
        .try_into()
        .expect("layout1_from always returns the fixed 1,344-byte ABI");
    if abi_program_vk != vkey.program_vk {
        return Err(LocalVerifyFailed);
    }
    match veritas::verify_zisk(&abi) {
        Ok(true) => Ok(()),
        _ => Err(LocalVerifyFailed),
    }
}

/// Builds the `PostRootProved` instruction for `params.anchor`'s candidate batch
/// (`root.head_pending_batch + 1`), or refuses BEFORE any send: continuity first (cheap, no
/// cryptography), then the local re-verify (the expensive check, still entirely local — `verify_zisk`
/// runs the real BN254 pairing on the host; the local re-verify is unconditional). Defence in
/// depth: the follower's resume gate already ran this same [`verify_checked`] before trusting a cached
/// artefact, but this send-time call is unconditional regardless of how `checked` was obtained.
pub fn build_post_ix(params: &PostParams) -> Result<Instruction, PreSendRefusal> {
    if *params.authority != params.anchor.root.authority {
        return Err(PreSendRefusal::NotTheAuthority {
            expected: params.anchor.root.authority,
            got: *params.authority,
        });
    }
    let pv = derive_public_values(params.checked)?;

    if pv.chain_id != params.anchor.root.chain_id {
        return Err(PreSendRefusal::ChainMismatch {
            expected: params.anchor.root.chain_id,
            got: pv.chain_id,
        });
    }

    // Bind the proof to the inbox batch it is about to be posted against, locally,
    // before any send — the same two facts `settle.rs::validate_post_root` refuses on chain
    // (`AccMismatch` / `OpenTsMismatch`), mirrored here at zero fee. A stale artefact (chain reset with
    // `work_dir` retained, a hand `--once` against another chain) is refused here, not after the fee.
    let inbox_batch = &params.anchor.inbox_batch_head_plus_1;
    if pv.inbox_commitment != inbox_batch.acc {
        return Err(PreSendRefusal::InboxCommitmentMismatch {
            expected: hex::encode(inbox_batch.acc),
            got: hex::encode(pv.inbox_commitment),
        });
    }
    if pv.open_unix_ts != inbox_batch.open_unix_ts as u64 {
        return Err(PreSendRefusal::OpenTsMismatch {
            expected: inbox_batch.open_unix_ts,
            got: pv.open_unix_ts,
        });
    }

    let (pred_last_block, pred_state_root) = predecessor_last_block_and_state_root(params.anchor);

    if pv.first_number != pred_last_block + 1 {
        return Err(PreSendRefusal::ContinuityMismatch {
            expected: pred_last_block + 1,
            got: pv.first_number,
        });
    }

    verify_checked(params.checked, params.vkey).map_err(|_| PreSendRefusal::LocalVerifyFailed)?;

    let abi =
        crate::abi::layout1_from(params.checked).map_err(|_| PreSendRefusal::LocalVerifyFailed)?;

    let batch = params.anchor.root.head_pending_batch + 1;
    let fields = crate::publics::to_post_root_fields(
        &pv,
        crate::publics::Anchor {
            batch,
            prev_batch: params.anchor.root.head_pending_batch,
            pre_state_root: pred_state_root,
        },
    );

    Ok(zk_settlement_client::post_root_proved_ix(
        params.settlement_program,
        params.authority,
        params.inbox_program,
        params.treasury,
        fields,
        abi,
        vec![],
    ))
}

/// Rounds a raw loaded-accounts byte requirement (`rome_zk_solana_sender::required_loaded_accounts_bytes`)
/// up to the next 32 KiB page — the same page size the V1 cost model itself charges in (`rome-zk-batcher`'s own
/// `tx_cost_units`). Pure; the caller supplies every account's live data length (`0` for the not-yet-created pending
/// PDA).
pub fn preflight_loaded_accounts_data_size_limit(account_data_lens: &[usize]) -> u32 {
    const PAGE: u32 = 32 * 1024;
    let raw = rome_zk_solana_sender::required_loaded_accounts_bytes(account_data_lens);
    raw.div_ceil(PAGE) * PAGE
}

/// A configured `loaded_accounts_data_size_limit` cannot cover what this send will actually load —
/// refused by name at start, never silently raised or silently sent anyway ("a configured Some(v) below the
/// derived value is refused by name").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "configured loaded_accounts_data_size_limit={configured} is below the {required}-byte requirement \
     this send's own live account lengths derive — raise the config (or unset it to derive automatically)"
)]
pub struct LoadedAccountsLimitTooLow {
    pub configured: u32,
    pub required: u32,
}

/// Resolves the `loaded_accounts_data_size_limit` a send actually uses: `None` in config means DERIVE
/// (the anchor's own live account lengths, via [`preflight_loaded_accounts_data_size_limit`]); a
/// configured `Some(v)` below that derived requirement is refused rather than silently raised or sent
/// anyway; a configured value at or above it is used as given (an operator's own headroom). The one
/// function both the dry-run header and the real send call — the 256 KiB literal this
/// replaces never appears again.
pub fn resolve_loaded_accounts_data_size_limit(
    configured: Option<u32>,
    account_data_lens: &[usize],
) -> Result<u32, LoadedAccountsLimitTooLow> {
    let derived = preflight_loaded_accounts_data_size_limit(account_data_lens);
    match configured {
        None => Ok(derived),
        Some(v) if v < derived => Err(LoadedAccountsLimitTooLow {
            configured: v,
            required: derived,
        }),
        Some(v) => Ok(v),
    }
}

/// `zk_settlement::errors::SettleError`'s own `#[repr(u32)]` discriminants, by name — pinned as a table
/// (never re-derived, and pinned with a test) since this crate does not
/// depend on the program crate itself. Kept in discriminant order; a code this table does not recognize
/// classifies as `PostFailed { name: "UnknownCustomError(N)" }` rather than panicking.
fn settle_error_name(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "BadBatchSequence",
        2 => "BadPrevBatch",
        3 => "BadPreStateRoot",
        4 => "BadFirstBlock",
        5 => "InboxNotFinalized",
        6 => "AccMismatch",
        7 => "WrongInboxAccount",
        8 => "MaxPendingReached",
        9 => "NotChainAuthority",
        10 => "BeforeDeadline",
        11 => "DisputesOpen",
        12 => "OutOfOrderFinality",
        13 => "NotPending",
        14 => "NotFinal",
        15 => "IsHeadPendingBatch",
        16 => "RegistryEntryNotFound",
        17 => "VkeyMismatch",
        18 => "UnsupportedLayout",
        19 => "HeaderHashMismatch",
        20 => "RegistryIndexOutOfRange",
        21 => "NotImplemented",
        22 => "WrongInboxProgram",
        23 => "StateRootMismatch",
        24 => "ParentHashMismatch",
        25 => "NotRegistryAuthority",
        26 => "ReservedIdNotAllowed",
        27 => "PermissionlessInitDisabled",
        28 => "BadPermissionlessChainId",
        29 => "WrongGlobalConfig",
        30 => "WrongChainConfig",
        31 => "WrongTreasury",
        32 => "GlobalConfigAlreadyInitialized",
        33 => "NoDepositToRefund",
        34 => "RefundNotYetEligible",
        35 => "ChainNotReclaimable",
        36 => "WrongChainAuthority",
        37 => "FeeOverflow",
        38 => "ChainAlreadyMigrated",
        39 => "NotUpgradeAuthority",
        40 => "TreasuryNotRentExempt",
        41 => "GasInBatchMismatch",
        42 => "InvalidRegistryAuthority",
        43 => "NotPendingRegistryAuthority",
        44 => "ReclaimWindowTooShort",
        45 => "DepositRequiredForPermissionless",
        46 => "DriftBoundZero",
        47 => "PublicValuesChainMismatch",
        48 => "BadLastBlock",
        49 => "CommitmentMismatch",
        50 => "OpenTsMismatch",
        51 => "DriftBoundMismatch",
        52 => "DriftBoundUnset",
        53 => "RetiredInstruction",
        54 => "BadPublicValuesPacking",
        55 => "BadPublicValues",
        56 => "RegistryFull",
        57 => "ActivationInPast",
        58 => "UnknownLayout",
        59 => "UnknownCurveOrScheme",
        60 => "LayoutMismatch",
        61 => "DuplicateRegistryEntry",
        62 => "EntryRetired",
        _ => return None,
    })
}

const BAD_BATCH_SEQUENCE: u32 = 1;

/// A failed send's outcome, classified by the on-chain program's own error name — never a raw error
/// code left for the caller to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostRefusal {
    /// `BadBatchSequence`, and a re-anchor shows `head_pending_batch >= our_batch` — this batch is
    /// already posted (by us, on a retry, or in principle by another poster); treat as success, not
    /// failure.
    AlreadyPosted,
    /// `BadBatchSequence`, and a re-anchor shows the chain head has moved STRICTLY PAST our batch
    /// (`head_pending_batch_after_reanchor > our_batch`) — one or more OTHER batches landed ahead of
    /// ours (another poster, or our own retry landing out of order); re-anchor and pick a fresh
    /// candidate, this job is stale.
    HeadAhead,
    /// `BadBatchSequence`, and a re-anchor still shows the chain head BEHIND our batch
    /// (`head_pending_batch_after_reanchor < our_batch`) — the re-anchor's own FINALIZED read is stale
    /// relative to whatever actually rejected our send (FINALIZED commitment lags CONFIRMED);
    /// retry the re-anchor rather than treating this as "someone else moved ahead" (this used to be
    /// misclassified `HeadAhead`).
    StaleAnchor,
    /// The fee payer could not cover the transaction fee — a `TransactionError`-level refusal, never a
    /// program `Custom` code.
    PayerLow,
    /// Any other on-chain refusal, or a send-layer failure this classifier does not special-case —
    /// named from [`settle_error_name`] when the code is recognized, else `UnknownCustomError(N)` or the
    /// raw description given.
    PostFailed { name: String },
}

/// Classifies a failed send. `custom_code` is the program's own `InstructionError::Custom(N)` value if
/// the failure was a program-level refusal (`None` for a transaction-level failure, e.g. an expired
/// blockhash or an underfunded fee payer); `insufficient_funds` is set by the caller from the raw
/// `TransactionError` it received (this function takes no crate-generation-specific error type, so it
/// works unchanged against either Solana client generation's own `TransactionError` — see the module's
/// own callers). `head_pending_batch_after_reanchor`/`our_batch` disambiguate `BadBatchSequence` between
/// "already posted" and "head moved the other way".
pub fn classify_send_failure(
    custom_code: Option<u32>,
    insufficient_funds: bool,
    head_pending_batch_after_reanchor: u64,
    our_batch: u64,
) -> PostRefusal {
    if insufficient_funds {
        return PostRefusal::PayerLow;
    }
    match custom_code {
        Some(BAD_BATCH_SEQUENCE) => match head_pending_batch_after_reanchor.cmp(&our_batch) {
            std::cmp::Ordering::Equal => PostRefusal::AlreadyPosted,
            std::cmp::Ordering::Greater => PostRefusal::HeadAhead,
            std::cmp::Ordering::Less => PostRefusal::StaleAnchor,
        },
        Some(code) => PostRefusal::PostFailed {
            name: settle_error_name(code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("UnknownCustomError({code})")),
        },
        None => PostRefusal::PostFailed {
            name: "unclassified transaction error".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{anchor, SnapshotFetch};
    use crate::calldata::{check_against_record, from_zisk_proof_file};
    use rome_zk_layouts::public_values::{read as read_pv, unpack_zisk_outputs};
    use rome_zk_solana_sender::{SendTuning, Sender, SenderError};
    use rome_zk_testkit::{
        cursor_account, funded_keypair, program_test, send_measuring_cu, ProgramSpec,
    };
    use solana_sdk::{
        instruction::InstructionError, signature::Signer, transaction::TransactionError,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ===== Pure classifier tests (pin BadBatchSequence/StateRootMismatch/GasInBatchMismatch) =====

    /// `head == batch` is the only case that means "our own batch already
    /// posted" — success, not failure.
    #[test]
    fn bad_batch_sequence_with_head_exactly_at_our_batch_is_already_posted() {
        assert_eq!(
            classify_send_failure(Some(BAD_BATCH_SEQUENCE), false, 5, 5),
            PostRefusal::AlreadyPosted
        );
    }

    /// **Mutation target:** collapsing this back to `>=` (so `head > batch` also classifies
    /// `AlreadyPosted`) would hide the real "someone else moved ahead" case behind a false success.
    #[test]
    fn bad_batch_sequence_with_head_strictly_past_our_batch_is_head_ahead() {
        assert_eq!(
            classify_send_failure(Some(BAD_BATCH_SEQUENCE), false, 6, 5),
            PostRefusal::HeadAhead
        );
    }

    /// (Was previously misclassified `HeadAhead`.) A re-anchor still showing the
    /// head BEHIND our batch means the re-anchor's own FINALIZED read is stale, not that another batch
    /// moved ahead of ours.
    #[test]
    fn bad_batch_sequence_with_head_still_behind_our_batch_is_stale_anchor() {
        assert_eq!(
            classify_send_failure(Some(BAD_BATCH_SEQUENCE), false, 3, 5),
            PostRefusal::StaleAnchor
        );
    }

    #[test]
    fn state_root_mismatch_classifies_by_name() {
        assert_eq!(
            classify_send_failure(Some(23), false, 0, 1),
            PostRefusal::PostFailed {
                name: "StateRootMismatch".to_string()
            }
        );
    }

    #[test]
    fn gas_in_batch_mismatch_classifies_by_name() {
        assert_eq!(
            classify_send_failure(Some(41), false, 0, 1),
            PostRefusal::PostFailed {
                name: "GasInBatchMismatch".to_string()
            }
        );
    }

    #[test]
    fn insufficient_funds_is_payer_low_regardless_of_any_custom_code() {
        assert_eq!(
            classify_send_failure(None, true, 0, 1),
            PostRefusal::PayerLow
        );
    }

    #[test]
    fn an_unrecognized_code_names_itself_rather_than_panicking() {
        assert_eq!(
            classify_send_failure(Some(9999), false, 0, 1),
            PostRefusal::PostFailed {
                name: "UnknownCustomError(9999)".to_string()
            }
        );
    }

    /// A configured limit below the derived requirement is refused by name at start, never
    /// silently raised or sent anyway.
    #[test]
    fn a_configured_limit_below_the_derived_requirement_is_refused_by_name() {
        let lens = [133_000usize; 10];
        let derived = preflight_loaded_accounts_data_size_limit(&lens);
        let err = resolve_loaded_accounts_data_size_limit(Some(derived - 1), &lens).unwrap_err();
        assert_eq!(
            err,
            LoadedAccountsLimitTooLow {
                configured: derived - 1,
                required: derived
            }
        );
    }

    #[test]
    fn none_derives_and_some_at_or_above_the_derived_value_is_used_as_given() {
        let lens = [133_000usize; 10];
        let derived = preflight_loaded_accounts_data_size_limit(&lens);
        assert_eq!(
            resolve_loaded_accounts_data_size_limit(None, &lens).unwrap(),
            derived
        );
        assert_eq!(
            resolve_loaded_accounts_data_size_limit(Some(derived), &lens).unwrap(),
            derived
        );
        assert_eq!(
            resolve_loaded_accounts_data_size_limit(Some(derived + 32 * 1024), &lens).unwrap(),
            derived + 32 * 1024
        );
    }

    #[test]
    fn preflight_rounds_up_to_the_next_32_kib_page() {
        assert_eq!(preflight_loaded_accounts_data_size_limit(&[]), 0);
        assert_eq!(preflight_loaded_accounts_data_size_limit(&[1]), 32 * 1024);
        // 10 accounts of 133,000 B each: raw = 10 * (64 + 133000) = 1,330,640 -> ceil to 41 pages.
        let raw = rome_zk_solana_sender::required_loaded_accounts_bytes(&[133_000; 10]);
        let want_pages = (raw as u64).div_ceil(32 * 1024);
        assert_eq!(
            preflight_loaded_accounts_data_size_limit(&[133_000; 10]) as u64,
            want_pages * 32 * 1024
        );
    }

    // ===== Real-BPF gate: the gate fixture's proof posts the real gate batch end to end =====

    fn gate_proof_bytes() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin"
        ))
        .expect("fixtures/prover-input/txv1-dev-reset6-batch-1.plonk.bin")
    }

    fn tiber_vkey() -> VkeyOfRecord {
        VkeyOfRecord::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/vkeys/tiber-200101-layout1.json"
        )))
        .expect("load vkey of record")
    }

    fn decode_gate_pv() -> PublicValues {
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let pv_bytes = unpack_zisk_outputs(&cd.publics_512).expect("unpack");
        read_pv(&pv_bytes).expect("read PublicValues")
    }

    const GATE_CHAIN_ID: u64 = 200_101;
    /// The gate fixture's sidecar `inbox_commitment` (`fixtures/prover-input/txv1-dev-reset6-batch-1.json`) —
    /// the inbox batch account's own `acc` the poster's `PostRootFields.inbox_commitment` must equal.
    const GATE_ACC_HEX: &str = "0774682b67c04292e4614fb5bf8540c3044208cacb2469c73f0613e515859b07";
    const GATE_OPEN_UNIX_TS: i64 = 1_789_413_402;

    /// Real fixture chunk count for the inbox batch account's own `expected_count` — post_root_proved
    /// never reads this field (only `chain_id`/`batch`/`finalized`/`acc`), so its exact value has no
    /// bearing on correctness; it is derived from the gate fixture's real input size purely so the
    /// account this test seeds is a realistic one, not a functional requirement.
    fn gate_expected_chunk_count() -> u32 {
        const MAX_FRAME_BODY_LEN: usize = 3_681; // the design-max frame body.
        let input_len = gate_proof_bytes().len().max(
            std::fs::metadata(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/prover-input/txv1-dev-reset6-batch-1.bin"
            ))
            .map(|m| m.len() as usize)
            .unwrap_or(0),
        );
        (input_len.div_ceil(MAX_FRAME_BODY_LEN)).max(1) as u32
    }

    fn encode_inbox_batch_account(finalized: bool, settlement_program: Pubkey) -> Vec<u8> {
        let acc: [u8; 32] = hex::decode(GATE_ACC_HEX).unwrap().try_into().unwrap();
        rome_zk_layouts::batch::write_header(&rome_zk_layouts::batch::BatchFields {
            chain_id: GATE_CHAIN_ID,
            batch: 1,
            open_slot: 1,
            expected_count: gate_expected_chunk_count(),
            leaves_present: gate_expected_chunk_count(),
            finalized,
            settlement_program: settlement_program.to_bytes(),
            authority: [0u8; 32],
            root: [0u8; 32],
            forced_root: [0u8; 32],
            acc,
            finalize_cursor: gate_expected_chunk_count(),
            open_unix_ts: GATE_OPEN_UNIX_TS,
        })
        .to_vec()
    }

    fn encode_registry_v2(inbox_program: Pubkey, vkey: [u8; 32], activation_slot: u64) -> Vec<u8> {
        use rome_zk_layouts::registry::*;
        let mut d = vec![0u8; REGISTRY_LEN_V2];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&GATE_CHAIN_ID.to_le_bytes());
        d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32].copy_from_slice(inbox_program.as_ref());
        d[OFF_COUNT] = 1;
        let e = OFF_ENTRIES;
        d[e] = CURVE_BN254;
        d[e + 1] = SCHEME_PLONK;
        d[e + 2..e + 34].copy_from_slice(&vkey);
        d[e + 34] = LAYOUT_ZISK_V1;
        let a = OFF_ACTIVATION;
        d[a..a + 8].copy_from_slice(&activation_slot.to_le_bytes());
        d
    }

    fn encode_chain_config_v2(max_drift_secs: u64) -> Vec<u8> {
        use rome_zk_layouts::chain_config::*;
        let mut d = vec![0u8; LEN_V2];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&GATE_CHAIN_ID.to_le_bytes());
        d[OFF_MAX_DRIFT_SECS..OFF_MAX_DRIFT_SECS + 8]
            .copy_from_slice(&max_drift_secs.to_le_bytes());
        d
    }

    /// A fake `SnapshotFetch` over a plain `HashMap`, refreshed from `solana-program-test`'s own bank
    /// between anchor calls — anchoring stays a pure, one-round-trip function; only the ACCOUNTS come
    /// from a real BPF-executing bank.
    struct BankSnapshot {
        accounts: HashMap<Pubkey, Vec<u8>>,
        slot: u64,
    }
    impl SnapshotFetch for BankSnapshot {
        fn get_multiple_accounts(
            &mut self,
            keys: &[Pubkey],
        ) -> Result<(u64, Vec<Option<Vec<u8>>>), crate::anchor::FetchError> {
            Ok((
                self.slot,
                keys.iter().map(|k| self.accounts.get(k).cloned()).collect(),
            ))
        }
    }

    /// Everything the gate scenario needs, wired against real BPF `zk_settlement.so`: root at the
    /// genesis sentinel (`head_pending_batch == head_final_batch == 0`, `number == 0`, `state_root ==
    /// [0;32]` — the sentinel, matching `to_post_root_fields`'s own `pre_state_root: [0;32]` test),
    /// chain_config v2 drift 60, a layout-1 registry entry for the gate fixture's own programVK, the
    /// inbox batch 1 account (finalized, `acc` == the sidecar's `inbox_commitment`), and the cursor at 2.
    struct GateRig {
        ctx: solana_program_test::ProgramTestContext,
        settlement_program: Pubkey,
        inbox_program: Pubkey,
        authority: solana_sdk::signature::Keypair,
        treasury: Pubkey,
    }
    impl GateRig {
        async fn start() -> Self {
            let settlement_program = Pubkey::new_unique();
            let inbox_program = Pubkey::new_unique();
            let authority = funded_keypair();
            let treasury = Pubkey::new_unique();
            // `upgradeable`, not `new`: this rig's own `anchor()` call now reads the settlement program's
            // real loader-v3 Program + ProgramData accounts — a plain `add_program`
            // (bpf_loader-owned) has neither.
            let mut pt = program_test(
                &[ProgramSpec::upgradeable(
                    "zk_settlement",
                    settlement_program,
                )],
                true,
            );
            pt.add_account(
                authority.pubkey(),
                solana_sdk::account::Account {
                    lamports: 50_000_000_000,
                    data: vec![],
                    owner: solana_system_interface::program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            );
            pt.add_account(
                treasury,
                solana_sdk::account::Account {
                    lamports: rome_zk_testkit::rent_exempt(0),
                    data: vec![],
                    owner: solana_system_interface::program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let (root_pda, _) = zk_settlement_client::root_pda(&settlement_program, GATE_CHAIN_ID);
            pt.add_account(
                root_pda,
                rome_zk_testkit::root_account_with_authority(
                    GATE_CHAIN_ID,
                    &authority.pubkey(),
                    settlement_program,
                ),
            );
            // root_account_with_authority only sets chain_id/authority — max_pending must be nonzero or
            // MaxPendingReached fires before anything else does; patch it directly at its own offset.
            let (registry_pda, _) =
                zk_settlement_client::registry_pda(&settlement_program, GATE_CHAIN_ID);
            pt.add_account(
                registry_pda,
                solana_sdk::account::Account {
                    lamports: rome_zk_testkit::rent_exempt(
                        rome_zk_layouts::registry::REGISTRY_LEN_V2,
                    ),
                    data: encode_registry_v2(inbox_program, tiber_vkey().program_vk, 0),
                    owner: settlement_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let (cc_pda, _) =
                zk_settlement_client::chain_config_pda(&settlement_program, GATE_CHAIN_ID);
            pt.add_account(
                cc_pda,
                solana_sdk::account::Account {
                    lamports: rome_zk_testkit::rent_exempt(rome_zk_layouts::chain_config::LEN_V2),
                    data: encode_chain_config_v2(60),
                    owner: settlement_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let (global_pda, _) = zk_settlement_client::global_config_pda(&settlement_program);
            pt.add_account(
                global_pda,
                solana_sdk::account::Account {
                    lamports: rome_zk_testkit::rent_exempt(rome_zk_layouts::global_config::LEN),
                    data: encode_global_config(treasury),
                    owner: settlement_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );
            let (cursor_pda, _) =
                zk_inbox_client::cursor_pda(&inbox_program, &settlement_program, GATE_CHAIN_ID);
            pt.add_account(cursor_pda, cursor_account(inbox_program, GATE_CHAIN_ID, 2));
            let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
                &inbox_program,
                &settlement_program,
                GATE_CHAIN_ID,
                1,
            );
            let acc_data = encode_inbox_batch_account(true, settlement_program);
            pt.add_account(
                inbox_batch_pda,
                solana_sdk::account::Account {
                    lamports: rome_zk_testkit::rent_exempt(acc_data.len()),
                    data: acc_data,
                    owner: inbox_program,
                    executable: false,
                    rent_epoch: 0,
                },
            );

            let mut ctx = pt.start_with_context().await;
            // Patch max_pending and predecessor-irrelevant fields the plain fixture builder above left
            // zeroed: root_account_with_authority's account is already in the bank; overwrite its
            // max_pending in place (offset-poke, same pattern the crate's own tests use elsewhere).
            let mut root_acc = ctx
                .banks_client
                .get_account(root_pda)
                .await
                .unwrap()
                .unwrap();
            root_acc.data[rome_zk_layouts::root::OFF_MAX_PENDING
                ..rome_zk_layouts::root::OFF_MAX_PENDING + 4]
                .copy_from_slice(&16u32.to_le_bytes());
            // A proved root must chain to its predecessor: the genesis block hash the root holds is the
            // parent hash the real gate proof commits to (the chain's own block 0), as `InitChainV2`
            // would have written it for chain 200101.
            root_acc.data
                [rome_zk_layouts::root::OFF_BLOCK_HASH..rome_zk_layouts::root::OFF_BLOCK_HASH + 32]
                .copy_from_slice(&decode_gate_pv().parent_hash);
            ctx.set_account(&root_pda, &root_acc.into());

            GateRig {
                ctx,
                settlement_program,
                inbox_program,
                authority,
                treasury,
            }
        }

        async fn bank_snapshot(&mut self) -> BankSnapshot {
            let (root_pda, _) =
                zk_settlement_client::root_pda(&self.settlement_program, GATE_CHAIN_ID);
            let (registry_pda, _) =
                zk_settlement_client::registry_pda(&self.settlement_program, GATE_CHAIN_ID);
            let (cc_pda, _) =
                zk_settlement_client::chain_config_pda(&self.settlement_program, GATE_CHAIN_ID);
            let (cursor_pda, _) = zk_inbox_client::cursor_pda(
                &self.inbox_program,
                &self.settlement_program,
                GATE_CHAIN_ID,
            );
            let (pred_pda, _) =
                zk_settlement_client::pending_pda(&self.settlement_program, GATE_CHAIN_ID, 0);
            let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
                &self.inbox_program,
                &self.settlement_program,
                GATE_CHAIN_ID,
                1,
            );
            let (global_pda, _) = zk_settlement_client::global_config_pda(&self.settlement_program);
            let programdata_pda = zk_settlement_client::program_data_pda(&self.settlement_program);
            let keys = [
                root_pda,
                registry_pda,
                cc_pda,
                cursor_pda,
                pred_pda,
                inbox_batch_pda,
                global_pda,
                self.settlement_program,
                programdata_pda,
            ];
            let mut accounts = HashMap::new();
            for k in keys {
                if let Some(acc) = self.ctx.banks_client.get_account(k).await.unwrap() {
                    accounts.insert(k, acc.data);
                }
            }
            let slot = self.ctx.banks_client.get_root_slot().await.unwrap();
            BankSnapshot { accounts, slot }
        }
    }

    /// `global_config` layout: this test only needs
    /// `treasury` populated correctly and every other field at a value `charge_fee` accepts — a small
    /// hand-encoding scoped to this file only (the crate has no write helper for this account either).
    fn encode_global_config(treasury: Pubkey) -> Vec<u8> {
        use rome_zk_layouts::global_config::*;
        let mut d = vec![0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_TREASURY..OFF_TREASURY + 32].copy_from_slice(treasury.as_ref());
        d
    }

    /// A no-op `Sender` that only counts calls — proves a pre-send refusal never reaches the network.
    #[derive(Default)]
    struct CountingSender {
        calls: AtomicU32,
    }
    impl Sender for CountingSender {
        async fn send_and_confirm(
            &self,
            _instructions: &[Instruction],
            _tuning: SendTuning,
        ) -> Result<solana_signature::Signature, SenderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("must never be called for a pre-send refusal");
        }
    }

    fn custom_code(err: &TransactionError) -> Option<u32> {
        match err {
            TransactionError::InstructionError(_, InstructionError::Custom(c)) => Some(*c),
            _ => None,
        }
    }

    #[tokio::test]
    async fn the_gate_fixture_posts_end_to_end_and_finalizes_immediately() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let pv = decode_gate_pv();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");

        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let ix = build_post_ix(&params).expect("gate fixture must build a clean instruction");

        let authority = rig.authority.insecure_clone();
        let (result, cu, _logs) =
            send_measuring_cu(&mut rig.ctx, std::slice::from_ref(&ix), &authority, &[]).await;
        result.expect("the gate fixture's real proof must post successfully");
        assert!(cu <= 600_000, "CU {cu} exceeds the 600k budget");

        // Serialized V1 tx size <= 4,096 B — build the exact same V1 wire shape this crate's own sender would, via its
        // own `build_v1_tx` (never a second, hand-rolled builder). The signer must be the SAME key as the ix's own
        // authority signer (poster key == chain authority == the sender's own payer) — converted from the program-test
        // keypair via its raw bytes (both key types wrap the same ed25519 keypair byte layout).
        let secret: [u8; 32] = rig.authority.to_bytes()[..32].try_into().unwrap();
        let payer = solana_keypair::Keypair::new_from_array(secret);
        let tx = rome_zk_solana_sender::build_v1_tx(
            &payer,
            &[ix],
            700_000,
            256 * 1024,
            1_000,
            solana_hash::Hash::default(),
        )
        .expect("build V1 tx");
        let bytes = wincode::serialize(&tx).expect("serialize V1 tx");
        assert!(
            bytes.len() <= 4096,
            "V1 tx is {} bytes, over the 4,096-byte envelope",
            bytes.len()
        );

        let root_data = rig
            .ctx
            .banks_client
            .get_account(zk_settlement_client::root_pda(&rig.settlement_program, GATE_CHAIN_ID).0)
            .await
            .unwrap()
            .unwrap();
        let root = zk_settlement_client::decode_root_account(&root_data.data).unwrap();
        assert_eq!(
            root.head_final_batch, 1,
            "post_root_proved must be born Final"
        );
        assert_eq!(root.number, 60);
        assert_eq!(root.block_hash, pv.last_block_hash);
    }

    /// Real BPF gate: the derived limit from THIS rig's genuine settlement-program
    /// `ProgramData` (the real compiled `.so`, loaded via `add_upgradeable_program_to_genesis`, not a
    /// hand-picked literal) is at least `64 + programdata_len` — the dominant term of the requirement, and well past
    /// the CLI's old 262,144-byte default.
    #[tokio::test]
    async fn the_derived_limit_covers_the_real_programdata_length() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");

        // Read the real ProgramData length independently of `anchor()`'s own computation, so this test
        // does not just check the formula against itself.
        let programdata_pda = zk_settlement_client::program_data_pda(&rig.settlement_program);
        let programdata_len = rig
            .ctx
            .banks_client
            .get_account(programdata_pda)
            .await
            .unwrap()
            .expect("a genuine ProgramData account must exist for an upgradeable-loader program")
            .data
            .len();

        let derived = preflight_loaded_accounts_data_size_limit(&a.account_data_lens);
        assert!(
            derived as u64 >= 64 + programdata_len as u64,
            "derived {derived} must cover at least 64 + the real ProgramData length {programdata_len}"
        );
        assert!(
            derived > 262_144,
            "the real zk_settlement.so's own ProgramData ({programdata_len} B) must already exceed the \
             old 256 KiB (262,144-byte) literal this preflight replaces"
        );
    }

    #[tokio::test]
    async fn a_tampered_state_root_is_classified_state_root_mismatch() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let pv = decode_gate_pv();
        let mut fields = crate::publics::to_post_root_fields(
            &pv,
            crate::publics::Anchor {
                batch: 1,
                prev_batch: 0,
                pre_state_root: [0u8; 32],
            },
        );
        fields.state_root = [0xAAu8; 32]; // disagrees with pv.state_root the chain will decode
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");
        let abi = crate::abi::layout1_from(&checked).unwrap();
        let ix = zk_settlement_client::post_root_proved_ix(
            &rig.settlement_program,
            &rig.authority.pubkey(),
            &rig.inbox_program,
            &rig.treasury,
            fields,
            abi,
            vec![],
        );
        let authority = rig.authority.insecure_clone();
        let (result, _cu, _logs) = send_measuring_cu(&mut rig.ctx, &[ix], &authority, &[]).await;
        let err = result.expect_err("a tampered state_root must be refused on chain");
        let code = custom_code(&err).expect("expected a custom program error");
        assert_eq!(
            classify_send_failure(Some(code), false, 0, 1),
            PostRefusal::PostFailed {
                name: "StateRootMismatch".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_tampered_gas_in_batch_is_classified_gas_in_batch_mismatch() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let pv = decode_gate_pv();
        let mut fields = crate::publics::to_post_root_fields(
            &pv,
            crate::publics::Anchor {
                batch: 1,
                prev_batch: 0,
                pre_state_root: [0u8; 32],
            },
        );
        fields.gas_in_batch = pv.gas_used + 1;
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");
        let abi = crate::abi::layout1_from(&checked).unwrap();
        let ix = zk_settlement_client::post_root_proved_ix(
            &rig.settlement_program,
            &rig.authority.pubkey(),
            &rig.inbox_program,
            &rig.treasury,
            fields,
            abi,
            vec![],
        );
        let authority = rig.authority.insecure_clone();
        let (result, _cu, _logs) = send_measuring_cu(&mut rig.ctx, &[ix], &authority, &[]).await;
        let err = result.expect_err("a tampered gas_in_batch must be refused on chain");
        let code = custom_code(&err).expect("expected a custom program error");
        assert_eq!(
            classify_send_failure(Some(code), false, 0, 1),
            PostRefusal::PostFailed {
                name: "GasInBatchMismatch".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_second_post_of_the_same_batch_is_classified_already_posted() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let ix = build_post_ix(&params).unwrap();
        let authority = rig.authority.insecure_clone();
        let (result, _cu, _logs) =
            send_measuring_cu(&mut rig.ctx, std::slice::from_ref(&ix), &authority, &[]).await;
        result.expect("first post must succeed");

        // Re-send the SAME batch-1 instruction a second time — head_pending_batch is now 1, so
        // `batch(1) != head_pending_batch(1)+1` refuses BadBatchSequence.
        let (result2, _cu2, _logs2) = send_measuring_cu(&mut rig.ctx, &[ix], &authority, &[]).await;
        let err = result2.expect_err("a second post of the same batch must be refused");
        let code = custom_code(&err).expect("expected a custom program error");
        assert_eq!(code, BAD_BATCH_SEQUENCE);

        let root_data = rig
            .ctx
            .banks_client
            .get_account(zk_settlement_client::root_pda(&rig.settlement_program, GATE_CHAIN_ID).0)
            .await
            .unwrap()
            .unwrap();
        let root = zk_settlement_client::decode_root_account(&root_data.data).unwrap();
        assert_eq!(
            classify_send_failure(Some(code), false, root.head_pending_batch, 1),
            PostRefusal::AlreadyPosted
        );
    }

    /// Real BPF gate rig: tamper the RIG's real on-chain inbox
    /// batch account's own `acc` (not merely the decoded `Anchor` struct — the same account `anchor()`
    /// itself reads) so it disagrees with the gate proof's own packaged `inbox_commitment`. Before this
    /// fix: `build_post_ix` never compared the two, so the send would go out and only be refused by the
    /// program (`AccMismatch`) after the fee. After: refused locally, zero sends.
    #[tokio::test]
    async fn a_tampered_inbox_batch_acc_is_refused_locally_before_any_send() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
            &rig.inbox_program,
            &rig.settlement_program,
            GATE_CHAIN_ID,
            1,
        );
        let mut acc = rig
            .ctx
            .banks_client
            .get_account(inbox_batch_pda)
            .await
            .unwrap()
            .unwrap();
        use rome_zk_layouts::batch::OFF_ACC;
        acc.data[OFF_ACC] ^= 0xFF; // flip the accumulator's own leading byte
        rig.ctx.set_account(&inbox_batch_pda, &acc.into());

        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly — the tamper is a content mismatch, not a shape refusal");

        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let err = build_post_ix(&params)
            .expect_err("a tampered inbox acc must be refused before any send");
        assert!(
            matches!(err, PreSendRefusal::InboxCommitmentMismatch { .. }),
            "got {err:?}"
        );
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }

    /// Real BPF gate rig: same shape, tampering the inbox batch
    /// account's own committed `open_unix_ts` instead — the local mirror of `settle.rs`'s
    /// `OpenTsMismatch`.
    #[tokio::test]
    async fn a_tampered_inbox_batch_open_unix_ts_is_refused_locally_before_any_send() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let inbox_batch_pda = zk_settlement_client::inbox_batch_pda(
            &rig.inbox_program,
            &rig.settlement_program,
            GATE_CHAIN_ID,
            1,
        );
        let mut acc = rig
            .ctx
            .banks_client
            .get_account(inbox_batch_pda)
            .await
            .unwrap()
            .unwrap();
        use rome_zk_layouts::batch::OFF_OPEN_UNIX_TS;
        let wrong_ts: i64 = GATE_OPEN_UNIX_TS + 1;
        acc.data[OFF_OPEN_UNIX_TS..OFF_OPEN_UNIX_TS + 8].copy_from_slice(&wrong_ts.to_le_bytes());
        rig.ctx.set_account(&inbox_batch_pda, &acc.into());

        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");

        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let err = build_post_ix(&params)
            .expect_err("a tampered open_unix_ts must be refused before any send");
        assert_eq!(
            err,
            PreSendRefusal::OpenTsMismatch {
                expected: wrong_ts,
                got: GATE_OPEN_UNIX_TS as u64,
            }
        );
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_predecessor_last_block_off_by_one_is_a_continuity_mismatch_before_any_send() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let mut snap = rig.bank_snapshot().await;
        let mut a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");
        // Genesis sentinel: predecessor is the root's own number. Tamper it to 59 (should be 0) so
        // `pv.first_number (1) != pred_last_block(59)+1`.
        a.root.number = 59;

        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let err = build_post_ix(&params).expect_err("must refuse before any send");
        assert_eq!(
            err,
            PreSendRefusal::ContinuityMismatch {
                expected: 60,
                got: 1
            }
        );
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }

    /// A proof whose own packaged `chain_id` disagrees with the chain this anchor
    /// snapshot is for is refused BEFORE any send — `PostParams` has no separate `pv` for a caller to
    /// (accidentally or otherwise) supply a mismatched value for; the check runs against what
    /// `derive_public_values` decodes from `checked` itself.
    #[tokio::test]
    async fn a_chain_id_mismatch_between_the_proof_and_the_anchor_is_refused_before_any_send() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let mut snap = rig.bank_snapshot().await;
        let mut a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");
        // The gate proof's own packaged public values carry chain_id 200_101 (GATE_CHAIN_ID); tamper the
        // anchor's own decoded root to claim a different chain.
        a.root.chain_id = 999;

        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let err = build_post_ix(&params).expect_err("must refuse before any send");
        assert_eq!(
            err,
            PreSendRefusal::ChainMismatch {
                expected: 999,
                got: GATE_CHAIN_ID,
            }
        );
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }

    /// The signer must BE the chain authority the root names — otherwise the program refuses on chain
    /// (`WrongAuthority`) after a fee is spent and, on the real path, after the minutes-long prove.
    #[tokio::test]
    async fn a_signer_that_is_not_the_root_authority_is_refused_before_any_send() {
        let mut rig = GateRig::start().await;
        let vkey = tiber_vkey();
        let cd = from_zisk_proof_file(&gate_proof_bytes()).expect("decode gate proof");
        let checked = check_against_record(&cd, &vkey).expect("checks clean");

        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");

        let impostor = Pubkey::new_unique();
        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &impostor,
            treasury: &rig.treasury,
        };
        let err =
            build_post_ix(&params).expect_err("a non-authority signer must be refused locally");
        assert_eq!(
            err,
            PreSendRefusal::NotTheAuthority {
                expected: a.root.authority,
                got: impostor,
            }
        );
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_flipped_proof_byte_is_local_verify_failed_before_any_send() {
        let rig_ctx = GateRig::start().await;
        let vkey = tiber_vkey();
        let mut bytes = gate_proof_bytes();
        // Flip a byte inside `proof_bytes` (the first 768-byte field) — decodes fine (program_vk/root_c
        // untouched) but the pairing itself must fail.
        bytes[10] ^= 0xff;
        let cd = from_zisk_proof_file(&bytes).expect("decode (still structurally valid)");
        let checked =
            check_against_record(&cd, &vkey).expect("program_vk/root_c untouched by the flip");

        let mut rig = rig_ctx;
        let mut snap = rig.bank_snapshot().await;
        let a = anchor(
            &mut snap,
            &rig.settlement_program,
            &rig.inbox_program,
            GATE_CHAIN_ID,
            1,
            &vkey,
        )
        .expect("should anchor cleanly");
        let sender = CountingSender::default();
        let params = PostParams {
            anchor: &a,
            vkey: &vkey,
            checked: &checked,
            settlement_program: &rig.settlement_program,
            inbox_program: &rig.inbox_program,
            authority: &rig.authority.pubkey(),
            treasury: &rig.treasury,
        };
        let err = build_post_ix(&params).expect_err("a flipped proof byte must fail local verify");
        assert_eq!(err, PreSendRefusal::LocalVerifyFailed);
        assert_eq!(sender.calls.load(Ordering::SeqCst), 0);
    }
}
