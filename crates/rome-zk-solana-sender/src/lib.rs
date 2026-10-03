//! `rome-zk-solana-sender` — the `Sender` seam: submits a **V1** (SIMD-0385) transaction and tracks it to
//! confirmation, resubmitting with a fresh blockhash and a bumped (bounded) priority fee on expiry. Ships
//! only the RPC implementation ([`RpcSender`]) — a TPU/QUIC client is a follow-up that would implement the
//! same [`Sender`] trait. The batcher re-exports this crate as its own `sender` module (unchanged call
//! sites); the prover orchestrator's `PostRootProved` poster, the challenger and governance tooling are its
//! intended consumers for the same Solana send/confirm machinery — today only the batcher depends on it.
//!
//! ## V1 only — no v0 fallback
//! Every transaction this sender builds is `VersionedMessage::V1`: message-first wire (version byte
//! `0x81`), a 4,096-byte envelope, and the CU limit + loaded-accounts-data-size limit carried in the
//! message's own header **config mask** — never as `ComputeBudget` instructions (a V1 message has no
//! concept of them; an unset config field is zero, fail-closed, so every V1 tx this sender builds sets
//! both fields explicitly). This replaces the batcher's original v0 (`VersionedMessage::V0`,
//! `ComputeBudgetInstruction`-prefixed, 1,232-byte) transport, which the Tiber gate run rejected outright
//! (`VersionedTransaction too large: 1684 bytes (max encoded/raw 1644/1232)`) because the design's own
//! 3,681-byte frame body was never going to fit v0's wire limit — a premise the earlier code never checked.
//!
//! ## Measured design-max frame size
//! `Open` + one `Write` (the design's own max frame body, 3,681 B) + `Seal{len, body_hash}` (32 B added
//! by the `body_hash` binding) + `SealLeaf`, signed as one V1 transaction, is **4,084 B**
//! of the 4,096-B envelope — 12 B of headroom (measured by
//! `design_frame_v1_tx_fits_4096_and_carries_both_header_limits`, this module). The next lever if more
//! headroom is ever needed is dropping the priority-fee field, never the body cap.
//!
//! ## Where the Solana types are converted
//! Until the Agave crate bump the programs, their `solana-program-test` suites and the two
//! instruction-builder crates (`zk-inbox-client`, `zk-settlement-client`) sat on
//! `solana-program = 2.1.6`, while this crate sat on the newer V1 crates, so one place had to turn the
//! old types into the new ones. The bump moved the whole workspace to `solana-program = 4.1.0` (one
//! version for every manifest, held by `scripts/check-workspace-deps.sh`). The [`compat`] sub-module
//! stays as the single place where an `Instruction` or `Pubkey` built by those two client crates is
//! copied, field for field, into the type `solana_message::v1::Message::try_compile_with_config` takes.
//! It is used once, in [`build_tx`], right before a message is compiled; nothing about the bytes changes.
//! Whether the copies can be dropped now that the versions line up is a separate cleanup that the bump
//! did not take on. Everywhere else in this crate (`preflight.rs`, `resolve.rs`, `resume.rs`, `pipeline.rs`'s account
//! polling, `config.rs`'s `Pubkey`-typed fields) uses the `solana_program` types the client crates expose. Two crates
//! still carry a renamed import
//! (`solana-client` as `solana-client-v1`, `solana-transaction-status-client-types` as
//! `solana-transaction-status-client-types-v1`; see `Cargo.toml`).
//!
//! ## The per-frame DAG ("hop collapse"; **one stage per frame**)
//! A frame's on-chain plan is still an explicit [`FramePlan`] — an ordered list of [`Stage`]s, each stage
//! a list of mutually independent transactions — because the *engine* below ([`run_send_and_confirm_many`])
//! is a fully general multi-stage DAG executor and stays that way; what changed is what
//! the batcher's own `pipeline::plan_chunk` now ever hands it: **exactly one stage holding exactly one
//! transaction**, the whole chunk lane (`Open`, one `Write` of the frame's entire body up to 3,681 bytes,
//! `Seal`, `SealLeaf`) in a single V1 transaction (one V1 tx per frame, one hop).
//! The old three-hop split (`[Open+Write0]` then the remaining `Write`s then `[Seal+SealLeaf]`, needed
//! because v0's 1,232-byte limit could not hold a >830-byte frame in one transaction) is withdrawn: V1's
//! 4,096-byte envelope holds the design's own 3,681-byte-body frame with room to spare (measured: 4,052 B
//! — see this module's own `design_frame_v1_tx_fits_4096_and_carries_both_header_limits` test), so there
//! is nothing left to split. `OpenBatch`+`GrowBatch` stays one transaction, as before (unaffected by this
//! change beyond also moving to the V1 wire format).
//!
//! [`RpcSender::send_and_confirm_many`] drives every frame's (now single-stage) [`FramePlan`] concurrently,
//! bounded by `in_flight` **outstanding transactions** — with one tx per frame this is now simply a bound
//! on outstanding frames.
//!
//! ## What stays: the DAG sender infrastructure
//! The in-flight bound, ≤ 256-signature batched `getSignatureStatuses` polling, block-height-gated
//! resubmit with every superseded signature kept (a late-confirming original still wins over a
//! resubmit's duplicate `AccountInUse`), the resubmit counter, and per-stage (now: per-frame, since there
//! is only one stage) timing are all unchanged in behavior — only the wire format and the shape of what
//! `pipeline::plan_chunk` feeds in changed.
//!
//! ## `RpcOps`
//! [`RpcSender::send_and_confirm_many`]'s actual network calls — `sendTransaction`, batched
//! `getSignatureStatuses`, `getLatestBlockhash` (with its `last_valid_block_height`), and
//! `getBlockHeight` — are the four methods of [`RpcOps`] below, implemented for real against the V1
//! generation's [`solana_client_v1::nonblocking::rpc_client::RpcClient`] and, in this module's own tests,
//! against a scripted fake.

#![forbid(unsafe_code)]

pub mod compat {
    //! The one place an instruction or key from `zk-inbox-client`/`zk-settlement-client` is copied into the
    //! types the V1 message compiler takes — see the module doc above.
    //! Every conversion is a plain 32-byte round-trip (`Pubkey`/`Instruction` are the same bytes on the
    //! wire regardless of which client-side crate constructed them); nothing here is fallible.

    /// A `solana_program` `Pubkey` -> a `solana_pubkey::Pubkey` (bytes are identical either way).
    pub fn to_v1_pubkey(p: &solana_program::pubkey::Pubkey) -> solana_pubkey::Pubkey {
        solana_pubkey::Pubkey::new_from_array(p.to_bytes())
    }

    /// The reverse of [`to_v1_pubkey`] — used once, for [`super::RpcSender::pubkey`]: the sender's own
    /// signing key is a [`solana_keypair::Keypair`], but every instruction builder in
    /// `zk-inbox-client`/`zk-settlement-client` (and `pipeline::BatchTarget`) expects the
    /// `solana_program` `Pubkey` this crate uses everywhere else.
    pub fn from_v1_pubkey(p: &solana_pubkey::Pubkey) -> solana_program::pubkey::Pubkey {
        solana_program::pubkey::Pubkey::new_from_array(p.to_bytes())
    }

    /// A `solana_signature::Signature` (what every send in this module returns) -> the
    /// `solana_sdk::signature::Signature` a *reading* RPC call (e.g. the binary's own `getTransaction`
    /// CU lookup, `RpcClient::get_transaction_with_config` on the renamed `solana-client-v1`) requires — a plain 64-byte
    /// round-trip, same as the pubkey conversions above.
    pub fn to_legacy_signature(
        s: &solana_signature::Signature,
    ) -> solana_sdk::signature::Signature {
        let bytes: [u8; 64] = (*s).into();
        solana_sdk::signature::Signature::from(bytes)
    }

    /// An `Instruction` (as built by `zk-inbox-client`/`zk-settlement-client`) -> the
    /// `solana_instruction::Instruction` that `solana_message::v1::Message::try_compile_with_config`
    /// requires. Field-for-field: same `program_id`, same account list (pubkey + signer/writable flags,
    /// in the same order — order is significant, `Seal`/`SealLeaf` etc. index into it), same raw
    /// instruction `data` bytes (already Borsh-encoded by the builder; untouched here).
    pub fn to_v1_instruction(
        ix: &solana_program::instruction::Instruction,
    ) -> solana_instruction::Instruction {
        solana_instruction::Instruction {
            program_id: to_v1_pubkey(&ix.program_id),
            accounts: ix
                .accounts
                .iter()
                .map(|m| solana_instruction::AccountMeta {
                    pubkey: to_v1_pubkey(&m.pubkey),
                    is_signer: m.is_signer,
                    is_writable: m.is_writable,
                })
                .collect(),
            data: ix.data.clone(),
        }
    }
}

// `solana_client_v1`/`solana_transaction_status_client_types_v1` named a genuinely
// different, newer major version than this crate's own `solana-program`/`solana-sdk` generation before
// the crate bump; the whole workspace is now one Agave line, so these are plain path aliases onto the
// same `solana-client`/`solana-transaction-status-client-types` dependency declared in Cargo.toml — kept
// so every existing `solana_client_v1::`/`solana_transaction_status_client_types_v1::` call site below
// (and in this crate's own tests, `mod tests` uses `super::*`) needs no rename.
use solana_client as solana_client_v1;
use solana_transaction_status_client_types as solana_transaction_status_client_types_v1;

use solana_client_v1::nonblocking::rpc_client::RpcClient;
use solana_client_v1::rpc_config::RpcSendTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::v1;
use solana_message::VersionedMessage;
use solana_program::instruction::Instruction;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types_v1::TransactionStatus;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The Solana RPC node's own per-call cap on `getSignatureStatuses` (`MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS`
/// upstream) — [`RpcSender::send_and_confirm_many`] never asks for more than this many statuses in one
/// call, however many transactions are outstanding. Value is protocol-level and identical across
/// Solana client versions (256) — `resolve.rs`'s own `getMultipleAccounts` chunking
/// uses [`MAX_MULTIPLE_ACCOUNTS`] below for the same reason.
pub use solana_client_v1::rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS as MAX_SIGNATURE_STATUSES_PER_CALL;

/// The Solana RPC node's own per-call cap on `getMultipleAccounts` — shared here so any caller scanning
/// many chunk PDAs at once (e.g. `resolve.rs`'s touched-range check) chunks the same way this module
/// chunks signature-status polling. Value is protocol-level (100), identical in both client generations.
pub use solana_client_v1::rpc_request::MAX_MULTIPLE_ACCOUNTS;

/// reqwest's own `Display` for a transport-level RPC error appends
/// `" for url (<URL>)"` — and that URL can carry a secret (an API key in its query string). This crate,
/// and every crate downstream of it, must never let that reach a log line. Generic over `Display` so it
/// works on either Solana client's own `ClientError` (this crate's own `solana_client_v1`, and
/// `rome-zk-batcher`'s separate `solana_client` read client) without this crate depending on both.
///
/// Strips the URL suffix and everything after it, keeping the error kind/variant text intact — e.g.
/// `"error sending request for url (https://x.example/?api-key=SECRET): ..."` becomes
/// `"error sending request"`.
pub fn describe_rpc_error(e: &impl std::fmt::Display) -> String {
    let full = e.to_string();
    match full.find(" for url") {
        Some(idx) => full[..idx].to_string(),
        None => full,
    }
}

/// The one line `with_retry` logs per failed attempt — redacted through [`describe_rpc_error`] (the
/// error's `Debug`, and `ErrorKind`'s, embed reqwest's full request URL; neither is ever formatted).
pub(crate) fn describe_retry_failure(
    attempt: u32,
    e: &solana_client_v1::client_error::ClientError,
) -> String {
    format!(
        "rpc call failed (attempt {attempt}), retrying with backoff: {}",
        describe_rpc_error(e)
    )
}

#[derive(Debug, thiserror::Error)]
pub enum SenderError {
    #[error("rpc error: {message}")]
    Rpc {
        message: String,
        #[source]
        source: Box<solana_client_v1::client_error::ClientError>,
    },
    #[error("v1 message compile error: {0}")]
    Compile(#[from] solana_message::CompileError),
    #[error("v1 priority-fee overflow: cu={cu} price={price}")]
    PriorityFeeOverflow { cu: u32, price: u64 },
    #[error("gave up after {resubmits} resubmits over {elapsed:?} without confirmation")]
    ConfirmTimeout { resubmits: u32, elapsed: Duration },
    /// The stall backstop fired while the transaction had landed (it is at `confirmed`) but had not reached
    /// the required commitment. It did execute and may still finalize: this is not "nothing landed".
    #[error(
        "gave up after {resubmits} resubmits over {elapsed:?}: the transaction landed (confirmed) but had not reached the required commitment when the cluster's block height stalled; it may still finalize"
    )]
    LandedNotFinal { resubmits: u32, elapsed: Duration },
    #[error("frame {frame_index} stage {stage_index} tx {tx_index} failed on chain: {err:?}")]
    StepFailed {
        frame_index: usize,
        stage_index: usize,
        tx_index: usize,
        err: solana_transaction_error::TransactionError,
    },
    #[error(
        "gave up after {resubmits} resubmits over {elapsed:?}: {unconfirmed} of {total} frames never confirmed"
    )]
    BatchConfirmTimeout {
        resubmits: u32,
        elapsed: Duration,
        unconfirmed: usize,
        total: usize,
    },
    /// Batched counterpart of [`SenderError::LandedNotFinal`]: the stall backstop fired while `landed` of
    /// the outstanding transactions had landed (at `confirmed`) but not yet reached the required
    /// commitment. Those executed and may still finalize.
    #[error(
        "gave up after {resubmits} resubmits over {elapsed:?}: {landed} outstanding transactions had landed (confirmed) but not reached the required commitment when the cluster's block height stalled, and {unconfirmed} of {total} frames never confirmed; the landed ones may still finalize"
    )]
    BatchLandedNotFinal {
        resubmits: u32,
        elapsed: Duration,
        landed: usize,
        unconfirmed: usize,
        total: usize,
    },
}

/// Tuning shared by every send this sender makes — a slice of the batcher's own `config::Config`, kept separate so
/// tests can construct it directly without a full `Config`.
///
/// `compute_unit_limit` and `loaded_accounts_data_size_limit` are carried in every V1 message's own
/// header **config mask** — never as `ComputeBudget`
/// instructions, and never left unset (an unset V1 config field defaults to **zero**, fail-closed, so
/// omitting either would cap the transaction at 0 CU / 0 loaded-account bytes rather than "the runtime
/// default").
///
/// **Fast-finality defaults.** `confirm_commitment` is the level a transaction must reach before the
/// sender calls it sent: `finalized` by default, so "sent" means final (under Alpenglow `finalized` trails
/// `confirmed` by 0-1 slot, measured on devnet; under TowerBFT it trails by about 31 slots, so
/// choose `confirmed` only where that wait matters more than reorg safety). `confirm_timeout` is not a
/// resubmit trigger: both send paths (the single send runs through the batched loop as one frame) poll every
/// signature they have submitted, resubmit with a fresh blockhash and a bumped fee only once the latest
/// attempt's blockhash has expired (block height past its `last_valid_block_height`), and never resubmit a
/// transaction already at `confirmed` or better. `confirm_timeout` sets the overall ceiling instead: past
/// ten times it a send stops resubmitting, but keeps waiting on a transaction that has landed (it is at
/// `confirmed`, waiting for the commitment) or whose latest attempt's blockhash is still valid, and it
/// starts no queued frame or later stage for the first time. It gives up with an error only once every
/// outstanding attempt has expired without landing, so that error means nothing can still land. A backstop
/// ends the wait on a stalled cluster: when the block height has not advanced for twenty times
/// `confirm_timeout` (a stall, not wall time, so a slow cluster that is still advancing never fails a
/// send). If a transaction had landed by then, the error says so ([`SenderError::LandedNotFinal`] /
/// [`SenderError::BatchLandedNotFinal`]): it executed and may still finalize.
/// `status_poll_interval` is the pause between status checks of the single-shot [`Sender::send_and_confirm`]
/// (the batched path takes its own `poll_interval` argument).
#[derive(Debug, Clone, Copy)]
pub struct SendTuning {
    pub compute_unit_limit: u32,
    pub loaded_accounts_data_size_limit: u32,
    pub priority_fee_micro_lamports: u64,
    pub max_priority_fee_micro_lamports: u64,
    pub confirm_timeout: Duration,
    pub confirm_commitment: ConfirmCommitment,
    pub status_poll_interval: Duration,
}

/// The default `confirm_timeout`, in seconds (was 60 for a TowerBFT cluster; 15 after the fast-finality
/// re-measurement). Every binary's `confirm_timeout_secs` config default is this constant.
pub const DEFAULT_CONFIRM_TIMEOUT_SECS: u64 = 15;

/// The default pause between status checks of the single-shot confirm loop.
pub const DEFAULT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The commitment level a send must reach before it is reported sent. Only the two levels that mean
/// something here are selectable: `processed` can still be dropped, so it is refused at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfirmCommitment {
    #[default]
    Finalized,
    Confirmed,
}

impl ConfirmCommitment {
    pub fn as_config(self) -> CommitmentConfig {
        match self {
            Self::Finalized => CommitmentConfig::finalized(),
            Self::Confirmed => CommitmentConfig::confirmed(),
        }
    }
}

impl std::str::FromStr for ConfirmCommitment {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "finalized" => Ok(Self::Finalized),
            "confirmed" => Ok(Self::Confirmed),
            other => Err(format!(
                "confirm_commitment must be \"finalized\" or \"confirmed\", got {other:?}"
            )),
        }
    }
}

impl Default for SendTuning {
    /// The fast-finality defaults: `finalized`, 15 s, 500 ms. The compute/fee fields default to zero and
    /// are always set by the caller (a V1 message with an unset compute limit would cap the transaction
    /// at zero CU), so a literal always names them and takes the rest from here with `..Default::default()`.
    fn default() -> Self {
        Self {
            compute_unit_limit: 0,
            loaded_accounts_data_size_limit: 0,
            priority_fee_micro_lamports: 0,
            max_priority_fee_micro_lamports: 0,
            confirm_timeout: Duration::from_secs(DEFAULT_CONFIRM_TIMEOUT_SECS),
            confirm_commitment: ConfirmCommitment::default(),
            status_poll_interval: DEFAULT_STATUS_POLL_INTERVAL,
        }
    }
}

/// Whether a signature's status has reached `commitment` — the one predicate the batched and the
/// single-shot confirm loops share. (`TransactionStatus::satisfies_commitment` treats a rooted
/// transaction, `confirmations: None`, as finalized.)
pub(crate) fn landed_at(status: &TransactionStatus, commitment: ConfirmCommitment) -> bool {
    status.satisfies_commitment(commitment.as_config())
}

/// Doubles the priority fee on each resubmit, capped at `max` (a resubmit gets a fresh blockhash and a
/// higher, bounded priority fee). Pure so it is trivially unit-testable without any RPC. The value this
/// returns is still a µL/CU *price*; [`priority_lamports`] turns a price into the flat lamport total a V1
/// message's `priority_fee` config field actually carries.
pub fn bumped_priority_fee(current: u64, max: u64) -> u64 {
    current.saturating_mul(2).min(max).max(current.min(max))
}

/// `ceil(cu * price / 1_000_000)` — the flat lamport total a V1 message's `config.priority_fee` field
/// carries. Mirrors the Rome SDK's V1 batch `priority_lamports` exactly (that crate lives in
/// a different repo, so this is a small, independently-tested duplication of the same formula, not a
/// dependency on it) and the on-chain `Origin::priority_fee` ceil-div this repo's own settlement/inbox
/// programs read gas charges from.
pub fn priority_lamports(cu: u32, price_micro_lamports: u64) -> Result<u64, SenderError> {
    let micro_lamports = (cu as u128).saturating_mul(price_micro_lamports as u128);
    let lamports = micro_lamports.saturating_add(999_999) / 1_000_000;
    u64::try_from(lamports).map_err(|_| SenderError::PriorityFeeOverflow {
        cu,
        price: price_micro_lamports,
    })
}

/// One stage of one frame's on-chain plan: a list of **independent** transactions (each a list of
/// instructions, as `zk-inbox-client` builds them — see the module
/// doc's "Where the Solana types are converted") that may be submitted concurrently, in any order. Every
/// frame the batcher's own `pipeline::plan_chunk` builds now has exactly one stage holding exactly one
/// transaction (the whole chunk lane fits one V1 tx); this type stays general because [`run_send_and_confirm_many`]
/// itself is a general multi-stage DAG executor, and `OpenBatch`+`GrowBatch`'s own single-transaction plan
/// is unaffected either way.
pub type Stage = Vec<Vec<Instruction>>;

/// One frame's full instruction plan as an explicit DAG: an ordered list of [`Stage`]s.
/// Stage `k+1` is only ever submitted once every transaction in stage `k` has confirmed; there is no
/// ordering requirement *within* a stage. The batcher's own `pipeline::plan_chunk` builds this — always a
/// single stage of a single transaction, the degenerate case this DAG executor has always handled
/// correctly (and still does; see this module's tests).
pub type FramePlan = Vec<Stage>;

/// The seam a TPU/QUIC sender would also implement (only [`RpcSender`] ships). One call =
/// one logical transaction, sent, resubmitted as needed, and confirmed — or a bounded failure.
/// `instructions` is the instruction list `zk-inbox-client`/`zk-settlement-client` build;
/// converting it into a V1 message is this trait's implementation's job (see [`compat`]).
pub trait Sender: Send + Sync {
    fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> impl std::future::Future<Output = Result<Signature, SenderError>> + Send;

    /// `rome-zk-batcher`'s bounded posting window needs a generic — fake-driven-test friendly — way to
    /// send a batch's whole chunk lane, not just one transaction at a time. This drives
    /// every frame's [`FramePlan`] to completion, `in_flight` frames concurrently, each frame's own stages
    /// strictly in order (stage `k+1` of a frame only after every transaction in stage `k` of that SAME
    /// frame confirms) — frames are otherwise fully independent. **This default implementation** drives
    /// [`Self::send_and_confirm`] directly (one call per transaction, no batched signature-status polling,
    /// `resubmits`/`rpc_retries` always reported 0 — a plain `send_and_confirm` per transaction never
    /// resubmits or retries on its own) — correct for, and the only implementation, any `Sender` test
    /// double needs. [`RpcSender`] overrides this with its own real, batched-polling implementation below;
    /// this default's behavior is otherwise unchanged from what this trait always required of a `Sender`.
    fn send_and_confirm_many(
        &self,
        frames: &[FramePlan],
        tuning: SendTuning,
        in_flight: usize,
        _poll_interval: Duration,
        _status_batch_size: usize,
    ) -> impl std::future::Future<Output = Result<BatchSendOutcome, SenderError>> + Send {
        async move {
            let started = Instant::now();
            let in_flight = in_flight.max(1);
            let mut confirmed = Vec::with_capacity(frames.len());
            let mut total_steps = 0usize;

            for chunk in frames.chunks(in_flight) {
                let results =
                    futures_util::future::join_all(chunk.iter().map(|frame| async move {
                        let frame_started = Instant::now();
                        let mut stage_latencies = Vec::new();
                        let mut last_sig = None;
                        let mut steps = 0usize;
                        for stage in frame {
                            if stage.is_empty() {
                                continue;
                            }
                            let stage_started = Instant::now();
                            let sigs = futures_util::future::join_all(
                                stage.iter().map(|tx| self.send_and_confirm(tx, tuning)),
                            )
                            .await;
                            for sig in sigs {
                                last_sig = Some(sig?);
                                steps += 1;
                            }
                            stage_latencies.push(stage_started.elapsed());
                        }
                        Ok::<_, SenderError>((
                            ConfirmedFrame {
                                signature: last_sig
                                    .expect("a non-empty FramePlan has at least one transaction"),
                                confirm_latency: frame_started.elapsed(),
                                stage_latencies,
                            },
                            steps,
                        ))
                    }))
                    .await;
                for r in results {
                    let (cf, steps) = r?;
                    confirmed.push(cf);
                    total_steps += steps;
                }
            }

            Ok(BatchSendOutcome {
                frames: confirmed,
                total_steps,
                resubmits: 0,
                elapsed: started.elapsed(),
                rpc_retries: 0,
            })
        }
    }
}

/// The RPC calls [`run_send_and_confirm_many`] needs, and nothing else — modeled as a trait so this
/// module's own tests can drive the send/resubmit/confirm loop against a scripted fake instead of a real
/// network. [`RpcClient`] (the V1-generation
/// `solana_client_v1::nonblocking::rpc_client::RpcClient`) implements it for production use.
pub trait RpcOps: Send + Sync {
    /// Submits an already-built, already-signed V1 transaction; does not wait for confirmation.
    fn send_transaction(
        &self,
        tx: &VersionedTransaction,
    ) -> impl std::future::Future<Output = Result<Signature, SenderError>> + Send;

    /// Statuses for exactly the signatures given (the caller chunks to
    /// [`MAX_SIGNATURE_STATUSES_PER_CALL`], never this trait).
    fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> impl std::future::Future<Output = Result<Vec<Option<TransactionStatus>>, SenderError>> + Send;

    /// `(blockhash, last_valid_block_height)` — the pair `getLatestBlockhash` returns together.
    /// A resubmit decision must be gated on `last_valid_block_height`, instead of a wall-clock guess at
    /// when a blockhash might have expired.
    fn get_latest_blockhash(
        &self,
    ) -> impl std::future::Future<Output = Result<(Hash, u64), SenderError>> + Send;

    /// The cluster's current block height — compared against a per-attempt `last_valid_block_height` to
    /// decide whether that attempt's blockhash has actually expired.
    fn get_block_height(
        &self,
    ) -> impl std::future::Future<Output = Result<u64, SenderError>> + Send;
}

pub struct RpcSender {
    rpc: RpcClient,
    /// The signing key (`solana-keypair`). [`RpcSender::pubkey`] hands back the `solana_program`
    /// `Pubkey` this crate's instruction builders expect (see [`compat::from_v1_pubkey`]) — there is no
    /// second `Keypair` anywhere; it is never needed (only the pubkey bytes are, for instruction
    /// building, and the private key, for signing the V1 tx this struct itself builds).
    payer: Keypair,
    /// Every retry `with_retry` performs across this sender's four `RpcOps` methods —
    /// reset at the start of each [`RpcSender::send_and_confirm_many`] call and
    /// read back into [`BatchSendOutcome::rpc_retries`]; also accumulates across the single-shot
    /// [`Sender::send_and_confirm`] path (not surfaced there, no batch outcome to carry it).
    retries: std::sync::atomic::AtomicU64,
}

/// Per-HTTP-request timeout for the underlying RPC client (distinct from [`SendTuning::confirm_timeout`],
/// which bounds how long *this sender* waits for a submitted transaction to confirm before resubmitting).
/// Without an explicit timeout the HTTP client's default has no ceiling at all — a single slow or
/// silently-dropped connection to a congested public RPC endpoint then blocks that call indefinitely,
/// which no amount of this module's own resubmit/retry logic can ever recover from (a stuck `.await`
/// never returns control to check any deadline). Generous relative to typical devnet RPC latency
/// (sub-second) without being so short it fails a request that was merely slow.
/// `pub` so the batcher binary's own read `RpcClient` (never sent a transaction, only
/// account/`getTransaction` reads) can share this exact value rather than a second, independently-typed
/// literal — see `bin/rome-zk-batcher.rs`'s own read-client construction.
pub const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// SIMD-0186's own per-account overhead: every account a V1 transaction loads costs this many bytes
/// PLUS its own data length, whether or not that account is one of the transaction's own `AccountMeta`s
/// (a loader-v3 program's `ProgramData` account is the standing example — never in the account list,
/// always loaded).
pub const LOADED_ACCOUNT_BASE_BYTES: u64 = 64;

/// The generic SIMD-0186 loaded-accounts-size formula: `Σ(64 + len)` over every
/// account a transaction loads, given each one's own data length (`0` for an account that does not yet
/// exist, e.g. a PDA the same instruction is about to create). Callers assemble `account_data_lens` from
/// their own live reads (or, for an about-to-be-created account, `0`) — this function does no I/O and
/// makes no assumption about which accounts a particular instruction shape touches; `rome-zk-batcher`'s
/// own `loaded_accounts::required_loaded_accounts_data_size` keeps its own call site (frame-specific
/// cushion terms, `FRAME_HEADER_LEN`) but its flat "64 bytes per account" term is this same formula
/// applied with every length assumed `0`.
pub fn required_loaded_accounts_bytes(account_data_lens: &[usize]) -> u32 {
    let total: u64 = account_data_lens
        .iter()
        .map(|&len| LOADED_ACCOUNT_BASE_BYTES + len as u64)
        .sum();
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Re-exported so a caller classifying a failed send (`sender_error_transaction_error`'s own callers,
/// e.g. `rome-zk-prover`'s CLI) can match on `TransactionError`/`InstructionError`
/// without adding a second, independently-pinned dependency on the crate that defines them.
pub use solana_instruction_error::InstructionError;
pub use solana_transaction_error::TransactionError;

/// Extracts the on-chain `TransactionError` a failed send carries, whichever `SenderError` variant holds
/// it: `StepFailed` directly (this crate's own on-chain execution classification — both the batched
/// `send_and_confirm_many` path and the single-transaction `send_and_confirm` path
/// above), or `Rpc` when the wrapped `ClientError` carries one — either as `ErrorKind::TransactionError`
/// or inside a preflight rejection (`SendTransactionPreflightFailure`, reachable if a caller ever sends
/// with `skip_preflight: false`); `ClientError::get_transaction_error` handles both shapes.
/// `None` for every other variant (`Compile`, `PriorityFeeOverflow`, `ConfirmTimeout`,
/// `BatchConfirmTimeout`) — none of those carry a program-level refusal to classify.
pub fn sender_error_transaction_error(err: &SenderError) -> Option<TransactionError> {
    match err {
        SenderError::StepFailed { err, .. } => Some(err.clone()),
        // `ClientError::get_transaction_error` covers both shapes a program refusal can arrive in: a
        // transport-level `ErrorKind::TransactionError`, and a preflight rejection carried inside
        // `RpcResponseErrorData::SendTransactionPreflightFailure`.
        SenderError::Rpc { source, .. } => source.get_transaction_error(),
        _ => None,
    }
}

impl RpcSender {
    /// `payer` is the V1-generation `Keypair` (e.g. read via `solana_keypair::read_keypair_file` in the
    /// binary) — this struct is the one place that key is ever used to sign a real transaction.
    pub fn new(rpc_url: String, payer: Keypair) -> Self {
        Self {
            rpc: RpcClient::new_with_timeout_and_commitment(
                rpc_url,
                RPC_REQUEST_TIMEOUT,
                CommitmentConfig::confirmed(),
            ),
            payer,
            retries: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The `solana_program` `Pubkey` every instruction builder in this crate expects (`pipeline::BatchTarget.payer`,
    /// `preflight::run`'s `payer` argument, etc.) — derived from this sender's own signing
    /// key via [`compat::from_v1_pubkey`], never a second, independently-read key.
    pub fn pubkey(&self) -> solana_program::pubkey::Pubkey {
        compat::from_v1_pubkey(&self.payer.pubkey())
    }
}

/// Builds a **V1** transaction the exact same way every real send in this module does, exposed for a
/// caller that only needs to MEASURE the wire shape (serialized size, header limits) without sending it
/// — the prover's poster measures its own gate instruction's V1 size this way, never a second, hand-rolled
/// V1-message builder. Returns whatever this crate's own `wincode::serialize` would produce (the caller
/// never needs to name the return type; see the module's own size-measurement tests for the pattern).
pub fn build_v1_tx(
    payer: &Keypair,
    instructions: &[Instruction],
    compute_unit_limit: u32,
    loaded_accounts_data_size_limit: u32,
    priority_fee_micro_lamports: u64,
    blockhash: Hash,
) -> Result<VersionedTransaction, SenderError> {
    build_tx(
        payer,
        instructions,
        compute_unit_limit,
        loaded_accounts_data_size_limit,
        priority_fee_micro_lamports,
        blockhash,
    )
}

/// Builds a **V1** transaction — message-first wire, CU limit + loaded-accounts-data-size limit carried in
/// the header config mask, never `ComputeBudget` instructions — and signs it with `payer`. Shared by every
/// send path in this module (the single-shot [`Sender::send_and_confirm`], the batched
/// [`RpcSender::send_and_confirm_many`], and this module's own fake-transport tests, none of which need a
/// live RPC to build or sign). `instructions` is the list `zk-inbox-client`/
/// `zk-settlement-client` build; [`compat::to_v1_instruction`] is the one conversion point (module doc).
fn build_tx(
    payer: &Keypair,
    instructions: &[Instruction],
    compute_unit_limit: u32,
    loaded_accounts_data_size_limit: u32,
    priority_fee_micro_lamports: u64,
    blockhash: Hash,
) -> Result<VersionedTransaction, SenderError> {
    let v1_ixs: Vec<solana_instruction::Instruction> =
        instructions.iter().map(compat::to_v1_instruction).collect();
    let mut config = v1::TransactionConfig::empty()
        .with_compute_unit_limit(compute_unit_limit)
        .with_loaded_accounts_data_size_limit(loaded_accounts_data_size_limit);
    if priority_fee_micro_lamports > 0 {
        config = config.with_priority_fee(priority_lamports(
            compute_unit_limit,
            priority_fee_micro_lamports,
        )?);
    }
    let message =
        v1::Message::try_compile_with_config(&payer.pubkey(), &v1_ixs, blockhash, config)?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V1(message), &[payer])
        .expect("signing with the sender's own keypair for its own fee payer cannot fail");
    Ok(tx)
}

/// One frame's outcome from [`RpcSender::send_and_confirm_many`]: the signature of its *last* step (with
/// one stage per frame, this is simply the frame's only transaction — e.g. `SealLeaf`
/// is folded into the same tx as `Open`+`Write`+`Seal` now, so "last step" and "the transaction" coincide)
/// and the wall-clock latency from the frame's first submit to that final confirmation.
///
/// `confirm_latency` alone only gives an averaged
/// `elapsed / hop_count` guess at hop latency, and under breadth-first FIFO scheduling across many
/// interleaved frames that average is not the real per-hop figure. `stage_latencies` is the real thing:
/// one entry per non-empty stage this frame actually went through (with single-stage
/// frames, exactly one entry, equal to `confirm_latency`), each the wall-clock time from that stage's own
/// first submission to every one of its transactions confirming.
#[derive(Debug, Clone)]
pub struct ConfirmedFrame {
    pub signature: Signature,
    pub confirm_latency: Duration,
    pub stage_latencies: Vec<Duration>,
}

/// Result of a whole [`RpcSender::send_and_confirm_many`] call: one [`ConfirmedFrame`] per input frame,
/// in input order, plus pipeline-wide totals. `total_steps` is every individual Solana transaction sent
/// and confirmed across every frame — with a single stage per frame this equals
/// the frame count, but the field stays general (a multi-stage `FramePlan`, e.g. `OpenBatch`+`GrowBatch`,
/// still contributes more than one).
#[derive(Debug, Clone)]
pub struct BatchSendOutcome {
    pub frames: Vec<ConfirmedFrame>,
    pub total_steps: usize,
    pub resubmits: u32,
    pub elapsed: Duration,
    /// Every transient RPC-call retry `with_retry` performed across
    /// every `RpcOps` call this whole batch send made — 0 for any caller driving
    /// [`run_send_and_confirm_many`] directly against a fake/test transport (this field is always 0 when
    /// constructed here; [`RpcSender::send_and_confirm_many`] is the only caller that ever overwrites it,
    /// from its own real `with_retry` counter).
    pub rpc_retries: u32,
}

/// Bookkeeping for one outstanding (submitted, not yet confirmed) transaction — never exposed outside
/// this module; keyed by a monotonic slot id in the `slots` map, itself indexed by every signature ever
/// submitted for this transaction via `sig_index` (see [`run_send_and_confirm_many`]'s module doc).
/// `(stage_index, tx_index)` addresses exactly one transaction within its own frame's [`FramePlan`]
/// (`frames[frame_index][stage_index][tx_index]`) — stage `stage_index + 1` is only ever submitted once
/// *every* transaction of `stage_index` has confirmed (tracked by [`FrameProgress`], not here).
struct Outstanding {
    frame_index: usize,
    stage_index: usize,
    tx_index: usize,
    priority_fee: u64,
    /// The `last_valid_block_height` that came back with the blockhash used for the *most
    /// recent* attempt at this transaction — a resubmit is only sent once `getBlockHeight` has passed
    /// this, never on a wall-clock guess.
    last_valid_block_height: u64,
    /// Every signature ever submitted for this transaction, oldest first. A resubmit *appends* rather
    /// than replaces — `Open` is not idempotent, so if the original signature is still outstanding (not
    /// resubmitted away) and later confirms `Ok`, that must count as the transaction succeeding, even
    /// though a newer signature was also submitted in the meantime.
    signatures: Vec<Signature>,
}

/// One frame's progress through its own [`FramePlan`] — which stage it is currently on, and how many of
/// that stage's transactions are still outstanding (submitted-or-queued, not yet confirmed) before the
/// next stage may start. `first_submitted` is set once, at the frame's very first submission, and is what
/// [`ConfirmedFrame::confirm_latency`] measures from.
struct FrameProgress {
    stage_index: usize,
    pending_in_stage: usize,
    first_submitted: Option<Instant>,
    /// The instant each non-empty stage this frame goes through was first submitted — index
    /// `k` here is that frame's `k`-th *non-empty* stage (not raw `FramePlan` stage index), pushed
    /// exactly once, the first time any of that stage's transactions is submitted.
    stage_submitted_at: Vec<Instant>,
    /// The instant each of those same stages fully confirmed (every one of its transactions landed) —
    /// index-aligned with `stage_submitted_at`; `stage_confirmed_at[k] - stage_submitted_at[k]` is that
    /// stage's real hop latency.
    stage_confirmed_at: Vec<Instant>,
}

/// The first stage index at or after `from` whose stage actually holds a transaction — skips any empty
/// stage. Returns `frame.len()` if every remaining stage (including none at all) is empty — the frame is
/// complete.
fn first_nonempty_stage(frame: &FramePlan, mut from: usize) -> usize {
    while from < frame.len() && frame[from].is_empty() {
        from += 1;
    }
    from
}

impl RpcSender {
    /// Submits `frames` — each an explicit [`FramePlan`] DAG of stages, each stage a list of independent
    /// transactions — keeping at most `in_flight` **transactions** outstanding at once (with one
    /// stage per frame this is directly a bound on outstanding frames), and confirms every
    /// outstanding transaction **in batches**: one `getSignatureStatuses` call per `status_batch_size`
    /// (capped at [`MAX_SIGNATURE_STATUSES_PER_CALL`], the RPC node's own hard cap) outstanding signatures
    /// per poll tick, instead of one RPC round trip per signature.
    ///
    /// A transaction whose blockhash has expired (`getBlockHeight` past that attempt's own
    /// `last_valid_block_height`, not a wall-clock guess) is resubmitted with a fresh
    /// blockhash and a bumped (bounded) priority fee — [`bumped_priority_fee`], same as the single-shot
    /// [`Sender::send_and_confirm`]. An on-chain execution error for any transaction fails the whole call
    /// immediately, reporting the exact `(frame, stage, tx)` it happened at — *unless* another signature
    /// for that same transaction confirmed `Ok` in the same poll. An overall wall-clock
    /// ceiling of `tuning.confirm_timeout * 10` stops resubmission and the start of any queued frame or later
    /// stage; the call then keeps waiting only on frames that have landed or whose latest attempt can still
    /// land, and fails loudly once none can, or once the block height has stalled (not advanced for
    /// `tuning.confirm_timeout * 20`) rather than waiting forever on a wedged cluster. A landed
    /// transaction at the stall gets [`SenderError::BatchLandedNotFinal`], not "without confirmation".
    pub async fn send_and_confirm_many(
        &self,
        frames: &[FramePlan],
        tuning: SendTuning,
        in_flight: usize,
        poll_interval: Duration,
        status_batch_size: usize,
    ) -> Result<BatchSendOutcome, SenderError> {
        self.retries.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut outcome = run_send_and_confirm_many(
            self,
            &self.payer,
            frames,
            tuning,
            in_flight,
            poll_interval,
            status_batch_size,
        )
        .await?;
        // `run_send_and_confirm_many` (generic over any `RpcOps`, including test fakes with no
        // notion of retries) always sets this to 0 — this is the one caller with a real `with_retry`
        // counter to overwrite it with.
        outcome.rpc_retries = self.retries.load(std::sync::atomic::Ordering::Relaxed) as u32;
        Ok(outcome)
    }
}

/// The real send/resubmit/confirm loop, generic over [`RpcOps`] so this module's own tests can drive it
/// against a scripted fake — see the module doc. [`RpcSender::send_and_confirm_many`] is the production
/// entry point (`ops = &self.rpc`, a real V1-generation [`RpcClient`]).
async fn run_send_and_confirm_many<O: RpcOps>(
    ops: &O,
    payer: &Keypair,
    frames: &[FramePlan],
    tuning: SendTuning,
    in_flight: usize,
    poll_interval: Duration,
    status_batch_size: usize,
) -> Result<BatchSendOutcome, SenderError> {
    let total = frames.len();
    let started = Instant::now();
    // The give-up ceiling: past it nothing is resubmitted, but a transaction that has landed (is at
    // `confirmed`, waiting for the commitment) or whose latest attempt's blockhash is still valid keeps
    // being waited on. The backstop is a STALL limit, not wall time: the wait ends only when the cluster's
    // block height has not advanced for `stall_limit`, so a slow cluster that is still advancing never fails
    // a send that has landed or can still land (its attempt expires, or it finalizes, as blocks arrive).
    let overall_deadline = started + tuning.confirm_timeout * 10;
    let stall_limit = tuning.confirm_timeout * 20;
    let mut highest_height: Option<u64> = None;
    let mut height_advanced_at = started;
    let in_flight = in_flight.max(1);
    let status_batch_size = status_batch_size.clamp(1, MAX_SIGNATURE_STATUSES_PER_CALL);

    // `queue` holds every transaction ready to submit but not yet started: `(frame_index, stage_index,
    // tx_index)`. Initialized to every frame's own stage-0 transactions (with single-stage
    // plans, exactly one per frame). `progress` tracks, per frame, which stage it is
    // currently on and how many of that stage's transactions are still outstanding.
    let mut queue: std::collections::VecDeque<(usize, usize, usize)> =
        std::collections::VecDeque::new();
    let mut progress: Vec<FrameProgress> = Vec::with_capacity(total);
    for frame in frames {
        let stage_index = first_nonempty_stage(frame, 0);
        let pending_in_stage = if stage_index < frame.len() {
            frame[stage_index].len()
        } else {
            0
        };
        progress.push(FrameProgress {
            stage_index,
            pending_in_stage,
            first_submitted: None,
            stage_submitted_at: Vec::new(),
            stage_confirmed_at: Vec::new(),
        });
    }
    for (frame_index, frame) in frames.iter().enumerate() {
        let p = &progress[frame_index];
        for tx_index in 0..p.pending_in_stage {
            queue.push_back((frame_index, p.stage_index, tx_index));
        }
        debug_assert!(
            p.pending_in_stage > 0 || p.stage_index >= frame.len(),
            "a non-empty FramePlan must have at least one non-empty stage"
        );
    }

    // `slots` holds one live `Outstanding` per transaction currently in flight, keyed by a monotonic id
    // (never reused); `sig_index` maps every signature ever submitted (original + every resubmit) back to
    // its slot, so a late-confirming superseded signature is still recognized.
    let mut slots: HashMap<u64, Outstanding> = HashMap::new();
    let mut sig_index: HashMap<Signature, u64> = HashMap::new();
    let mut next_slot_id: u64 = 0;
    let mut confirmed: Vec<Option<ConfirmedFrame>> = vec![None; total];
    let mut resubmits: u32 = 0;
    let mut total_steps: usize = 0;

    // One shared blockhash (+ its `last_valid_block_height`) refreshed at most every `BLOCKHASH_REFRESH`.
    let (mut blockhash, mut last_valid_block_height) = ops.get_latest_blockhash().await?;
    let mut blockhash_fetched_at = Instant::now();

    while confirmed.iter().any(Option::is_none) {
        if blockhash_fetched_at.elapsed() > BLOCKHASH_REFRESH {
            let (h, lvbh) = ops.get_latest_blockhash().await?;
            blockhash = h;
            last_valid_block_height = lvbh;
            blockhash_fetched_at = Instant::now();
        }

        // Top up from the queue up to `in_flight` *transactions* outstanding. Gathered first, then
        // submitted as one concurrent wave (`join_all`, not a sequential loop of awaits).
        // Past the give-up ceiling nothing new is started (a queued frame or a later stage for the first
        // time): the send only waits on what is already submitted.
        let may_start = Instant::now() <= overall_deadline;
        let mut to_start = Vec::new();
        while may_start && slots.len() + to_start.len() < in_flight {
            let Some(unit) = queue.pop_front() else {
                break;
            };
            to_start.push(unit);
        }
        if !to_start.is_empty() {
            let now = Instant::now();
            let submits = to_start
                .iter()
                .map(|&(frame_index, stage_index, tx_index)| {
                    submit(
                        ops,
                        payer,
                        &frames[frame_index][stage_index][tx_index],
                        tuning.compute_unit_limit,
                        tuning.loaded_accounts_data_size_limit,
                        tuning.priority_fee_micro_lamports,
                        blockhash,
                    )
                });
            let results = futures_util::future::join_all(submits).await;
            for ((frame_index, stage_index, tx_index), result) in to_start.into_iter().zip(results)
            {
                let signature = result?;
                let fp = &mut progress[frame_index];
                if fp.first_submitted.is_none() {
                    fp.first_submitted = Some(now);
                }
                if fp.stage_submitted_at.len() == fp.stage_confirmed_at.len() {
                    fp.stage_submitted_at.push(now);
                }
                let slot_id = next_slot_id;
                next_slot_id += 1;
                sig_index.insert(signature, slot_id);
                slots.insert(
                    slot_id,
                    Outstanding {
                        frame_index,
                        stage_index,
                        tx_index,
                        priority_fee: tuning.priority_fee_micro_lamports,
                        last_valid_block_height,
                        signatures: vec![signature],
                    },
                );
            }
        }

        if slots.is_empty() {
            if queue.is_empty() {
                // Queue drained and nothing left in flight: every frame confirmed.
                break;
            }
            // Past the ceiling with work still queued that will not be started: the batch cannot finish.
            return Err(SenderError::BatchConfirmTimeout {
                resubmits,
                elapsed: started.elapsed(),
                unconfirmed: confirmed.iter().filter(|c| c.is_none()).count(),
                total,
            });
        }

        tokio::time::sleep(poll_interval).await;

        // Read the block height BEFORE the status pass, so a "not landed" status is always observed after
        // the expiry it is judged against is already known: a transaction that lands between the two reads
        // can then only look live, never look expired and unlanded.
        let block_height = ops.get_block_height().await?;
        if highest_height.is_none_or(|h| block_height > h) {
            highest_height = Some(block_height);
            height_advanced_at = Instant::now();
        }

        // One batched confirmation pass: chunk every signature outstanding to `status_batch_size`
        // (capped at the RPC node's own hard cap) and ask for all of them, instead of one call per
        // signature.
        let mut slot_ok: HashMap<u64, Signature> = HashMap::new();
        let mut slot_err: HashMap<u64, (Signature, solana_transaction_error::TransactionError)> =
            HashMap::new();
        // `slot_landed`: slots with at least one signature at `confirmed` or better (any result). Such a
        // transaction has landed; it is waited on, never resubmitted, however far the block height is.
        let mut slot_landed: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let sigs: Vec<Signature> = sig_index.keys().copied().collect();
        for chunk in sigs.chunks(status_batch_size) {
            let statuses = ops.get_signature_statuses(chunk).await?;
            for (signature, status) in chunk.iter().zip(statuses) {
                let Some(status) = status else { continue };
                let Some(&slot_id) = sig_index.get(signature) else {
                    continue;
                };
                if landed_at(&status, ConfirmCommitment::Confirmed) {
                    slot_landed.insert(slot_id);
                }
                if !landed_at(&status, tuning.confirm_commitment) {
                    continue;
                }
                match status.err {
                    None => {
                        slot_ok.entry(slot_id).or_insert(*signature);
                    }
                    Some(err) => {
                        slot_err.entry(slot_id).or_insert((*signature, err));
                    }
                }
            }
        }

        // The status pass above is several sequential calls, so the root can move between them: a
        // duplicate's failure may be read in one call and the original's `Ok` missed in an earlier one.
        // Before failing a slot that has more than one signature, ask about all of them together; any
        // `Ok` at the commitment wins.
        let to_reread: Vec<u64> = slot_err
            .keys()
            .filter(|id| !slot_ok.contains_key(id))
            .filter(|id| slots.get(id).is_some_and(|e| e.signatures.len() > 1))
            .copied()
            .collect();
        for id in to_reread {
            let all = slots[&id].signatures.clone();
            for chunk in all.chunks(MAX_SIGNATURE_STATUSES_PER_CALL) {
                let statuses = ops.get_signature_statuses(chunk).await?;
                for (signature, status) in chunk.iter().zip(statuses) {
                    if let Some(status) = status {
                        if status.err.is_none() && landed_at(&status, tuning.confirm_commitment) {
                            slot_ok.entry(id).or_insert(*signature);
                        }
                    }
                }
            }
        }

        for (slot_id, signature) in &slot_ok {
            let entry = slots
                .remove(slot_id)
                .expect("slot_ok only holds live slot ids");
            for s in &entry.signatures {
                sig_index.remove(s);
            }
            total_steps += 1;
            let fp = &mut progress[entry.frame_index];
            fp.pending_in_stage -= 1;
            if fp.pending_in_stage == 0 {
                fp.stage_confirmed_at.push(Instant::now());
                let next_stage =
                    first_nonempty_stage(&frames[entry.frame_index], entry.stage_index + 1);
                if next_stage >= frames[entry.frame_index].len() {
                    confirmed[entry.frame_index] = Some(ConfirmedFrame {
                        signature: *signature,
                        confirm_latency: fp
                            .first_submitted
                            .expect("a frame reaching completion must have been submitted at least once")
                            .elapsed(),
                        stage_latencies: fp
                            .stage_submitted_at
                            .iter()
                            .zip(fp.stage_confirmed_at.iter())
                            .map(|(s, c)| c.saturating_duration_since(*s))
                            .collect(),
                    });
                } else {
                    fp.stage_index = next_stage;
                    fp.pending_in_stage = frames[entry.frame_index][next_stage].len();
                    for tx_index in 0..fp.pending_in_stage {
                        queue.push_back((entry.frame_index, next_stage, tx_index));
                    }
                }
            }
        }
        for (slot_id, (_, err)) in &slot_err {
            if slot_ok.contains_key(slot_id) {
                continue; // an Ok for the same transaction arrived in the same poll — that wins.
            }
            let entry = slots
                .remove(slot_id)
                .expect("slot_err only holds live slot ids");
            return Err(SenderError::StepFailed {
                frame_index: entry.frame_index,
                stage_index: entry.stage_index,
                tx_index: entry.tx_index,
                err: err.clone(),
            });
        }

        // Resubmit (same transaction, not the next one) anything still outstanding whose *own* attempt's
        // blockhash has actually expired — except a transaction already at `confirmed` or better, which has
        // landed and is only waiting to reach the commitment. Nothing is resubmitted past the give-up ceiling.
        let past_ceiling = Instant::now() > overall_deadline;
        if !slots.is_empty() && !past_ceiling {
            let expired: Vec<u64> = slots
                .iter()
                .filter(|(id, e)| {
                    block_height > e.last_valid_block_height && !slot_landed.contains(id)
                })
                .map(|(id, _)| *id)
                .collect();
            if !expired.is_empty() {
                let (fresh_hash, fresh_last_valid) = ops.get_latest_blockhash().await?;
                blockhash = fresh_hash;
                last_valid_block_height = fresh_last_valid;
                blockhash_fetched_at = Instant::now();

                let submits = expired.iter().map(|id| {
                    let entry = &slots[id];
                    let fee = bumped_priority_fee(
                        entry.priority_fee,
                        tuning.max_priority_fee_micro_lamports,
                    );
                    submit(
                        ops,
                        payer,
                        &frames[entry.frame_index][entry.stage_index][entry.tx_index],
                        tuning.compute_unit_limit,
                        tuning.loaded_accounts_data_size_limit,
                        fee,
                        blockhash,
                    )
                });
                let results = futures_util::future::join_all(submits).await;
                for (id, result) in expired.iter().zip(results) {
                    let signature = result?;
                    resubmits += 1;
                    let entry = slots.get_mut(id).expect("expired ids are live slot ids");
                    entry.priority_fee = bumped_priority_fee(
                        entry.priority_fee,
                        tuning.max_priority_fee_micro_lamports,
                    );
                    entry.last_valid_block_height = last_valid_block_height;
                    entry.signatures.push(signature);
                    sig_index.insert(signature, *id);
                }
            }
        }

        // Give up only when nothing outstanding can still land: past the ceiling, every slot's latest
        // attempt has expired (block height past its `last_valid_block_height`) without having landed. A
        // block height that has not advanced for `stall_limit` ends the wait on a stalled cluster.
        let nothing_can_land = || {
            slots.iter().all(|(id, e)| {
                block_height > e.last_valid_block_height && !slot_landed.contains(id)
            })
        };
        let stalled = height_advanced_at.elapsed() > stall_limit;
        if !slots.is_empty() && ((past_ceiling && nothing_can_land()) || stalled) {
            let unconfirmed = confirmed.iter().filter(|c| c.is_none()).count();
            // A transaction at `confirmed` or better has executed and may still finalize: say so, rather
            // than "without confirmation".
            let landed = slots.keys().filter(|id| slot_landed.contains(id)).count();
            if landed > 0 {
                return Err(SenderError::BatchLandedNotFinal {
                    resubmits,
                    elapsed: started.elapsed(),
                    landed,
                    unconfirmed,
                    total,
                });
            }
            return Err(SenderError::BatchConfirmTimeout {
                resubmits,
                elapsed: started.elapsed(),
                unconfirmed,
                total,
            });
        }
    }

    Ok(BatchSendOutcome {
        frames: confirmed
            .into_iter()
            .map(|c| c.expect("every remaining frame confirmed or an error returned above"))
            .collect(),
        total_steps,
        resubmits,
        elapsed: started.elapsed(),
        rpc_retries: 0,
    })
}

/// The single-shot send, run through the batched machinery as one single-stage, single-transaction frame
/// (generic over [`RpcOps`], so the fake drives it). Every guarantee of [`run_send_and_confirm_many`]
/// carries over: every signature ever submitted is polled and an `Ok` on any of them wins; a transaction
/// already at `confirmed` or better is never resubmitted, only waited on; and a resubmit waits for the
/// attempt's own `last_valid_block_height` to pass rather than a wall-clock guess. The two error shapes
/// the single-shot callers match on are kept: a wall-clock give-up is [`SenderError::ConfirmTimeout`], and
/// an on-chain failure is [`SenderError::StepFailed`] at `(0, 0, 0)`.
async fn run_send_and_confirm_one<O: RpcOps>(
    ops: &O,
    payer: &Keypair,
    instructions: &[Instruction],
    tuning: SendTuning,
) -> Result<Signature, SenderError> {
    let frame: FramePlan = vec![vec![instructions.to_vec()]];
    match run_send_and_confirm_many(
        ops,
        payer,
        std::slice::from_ref(&frame),
        tuning,
        1,
        tuning.status_poll_interval,
        MAX_SIGNATURE_STATUSES_PER_CALL,
    )
    .await
    {
        Ok(outcome) => Ok(outcome
            .frames
            .into_iter()
            .next()
            .expect("one frame in, one frame out")
            .signature),
        Err(SenderError::BatchConfirmTimeout {
            resubmits, elapsed, ..
        }) => Err(SenderError::ConfirmTimeout { resubmits, elapsed }),
        Err(SenderError::BatchLandedNotFinal {
            resubmits, elapsed, ..
        }) => Err(SenderError::LandedNotFinal { resubmits, elapsed }),
        Err(e) => Err(e),
    }
}

/// Builds, signs, and submits (no confirmation wait) one step against a caller-supplied blockhash —
/// shared by every submit/resubmit path in [`run_send_and_confirm_many`].
#[allow(clippy::too_many_arguments)]
async fn submit<O: RpcOps>(
    ops: &O,
    payer: &Keypair,
    instructions: &[Instruction],
    compute_unit_limit: u32,
    loaded_accounts_data_size_limit: u32,
    priority_fee_micro_lamports: u64,
    blockhash: Hash,
) -> Result<Signature, SenderError> {
    let tx = build_tx(
        payer,
        instructions,
        compute_unit_limit,
        loaded_accounts_data_size_limit,
        priority_fee_micro_lamports,
        blockhash,
    )?;
    ops.send_transaction(&tx).await
}

impl RpcOps for RpcSender {
    async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<Signature, SenderError> {
        with_retry(&self.retries, || {
            RpcClient::send_transaction_with_config(
                &self.rpc,
                tx,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
        })
        .await
        .map_err(|e| SenderError::Rpc {
            message: describe_rpc_error(&e),
            source: Box::new(e),
        })
    }

    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<TransactionStatus>>, SenderError> {
        with_retry(&self.retries, || {
            RpcClient::get_signature_statuses(&self.rpc, signatures)
        })
        .await
        .map(|r| r.value)
        .map_err(|e| SenderError::Rpc {
            message: describe_rpc_error(&e),
            source: Box::new(e),
        })
    }

    async fn get_latest_blockhash(&self) -> Result<(Hash, u64), SenderError> {
        with_retry(&self.retries, || {
            RpcClient::get_latest_blockhash_with_commitment(
                &self.rpc,
                CommitmentConfig::confirmed(),
            )
        })
        .await
        .map_err(|e| SenderError::Rpc {
            message: describe_rpc_error(&e),
            source: Box::new(e),
        })
    }

    async fn get_block_height(&self) -> Result<u64, SenderError> {
        with_retry(&self.retries, || RpcClient::get_block_height(&self.rpc))
            .await
            .map_err(|e| SenderError::Rpc {
                message: describe_rpc_error(&e),
                source: Box::new(e),
            })
    }
}

/// How long a fetched blockhash is trusted for before [`run_send_and_confirm_many`] fetches a new one —
/// a blockhash stays usable for ~60-90s on Solana, so this is a conservative fraction of that.
const BLOCKHASH_REFRESH: Duration = Duration::from_secs(20);

/// Retries a transient RPC failure (rate limiting, a dropped connection) with exponential backoff —
/// this module's own resilience against a public, rate-limited RPC endpoint, distinct from
/// [`Sender::send_and_confirm`]'s resubmit-on-blockhash-expiry.
async fn with_retry<T, Fut>(
    retries: &std::sync::atomic::AtomicU64,
    mut f: impl FnMut() -> Fut,
) -> Result<T, solana_client_v1::client_error::ClientError>
where
    Fut: std::future::Future<Output = Result<T, solana_client_v1::client_error::ClientError>>,
{
    const ATTEMPTS: u32 = 6;
    const BASE_DELAY: Duration = Duration::from_millis(300);
    let mut attempt = 0u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt + 1 < ATTEMPTS => {
                attempt += 1;
                retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Never `?e` / `?e.kind()`: reqwest's `Debug` carries the full request URL (api-key query
                // string included). One redacting helper, tested.
                tracing::warn!("{}", describe_retry_failure(attempt, &e));
                tokio::time::sleep(BASE_DELAY * 2u32.pow(attempt.min(4))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

impl Sender for RpcSender {
    /// Overrides the trait's generic default with this struct's own real, batched-signature-status-polling
    /// implementation (unchanged from before this trait method existed) — the in-flight bound, block-
    /// height-gated resubmit, and the retry counter this module's own doc describes.
    async fn send_and_confirm_many(
        &self,
        frames: &[FramePlan],
        tuning: SendTuning,
        in_flight: usize,
        poll_interval: Duration,
        status_batch_size: usize,
    ) -> Result<BatchSendOutcome, SenderError> {
        RpcSender::send_and_confirm_many(
            self,
            frames,
            tuning,
            in_flight,
            poll_interval,
            status_batch_size,
        )
        .await
    }

    async fn send_and_confirm(
        &self,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<Signature, SenderError> {
        run_send_and_confirm_one(self, &self.payer, instructions, tuning).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_transaction_error::TransactionError;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// Upper bound on one run of the confirm loop in a test. A regression that stops the loop from ending
    /// (a lost backstop, a wait with no exit) fails the test by name instead of hanging the CI job. The
    /// tests' own legitimate runs end in well under 10 s.
    const LOOP_BOUND: Duration = Duration::from_secs(30);

    fn running_test() -> String {
        std::thread::current().name().unwrap_or("?").to_string()
    }

    /// Test-module shadows of the two loop entry points: same call, bounded by [`LOOP_BOUND`].
    async fn run_send_and_confirm_many<O: RpcOps>(
        ops: &O,
        payer: &Keypair,
        frames: &[FramePlan],
        tuning: SendTuning,
        in_flight: usize,
        poll_interval: Duration,
        status_batch_size: usize,
    ) -> Result<BatchSendOutcome, SenderError> {
        match tokio::time::timeout(
            LOOP_BOUND,
            super::run_send_and_confirm_many(
                ops,
                payer,
                frames,
                tuning,
                in_flight,
                poll_interval,
                status_batch_size,
            ),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => panic!(
                "{}: the batched confirm loop did not end within {LOOP_BOUND:?} (a regression in the give-up path)",
                running_test()
            ),
        }
    }

    async fn run_send_and_confirm_one<O: RpcOps>(
        ops: &O,
        payer: &Keypair,
        instructions: &[Instruction],
        tuning: SendTuning,
    ) -> Result<Signature, SenderError> {
        match tokio::time::timeout(
            LOOP_BOUND,
            super::run_send_and_confirm_one(ops, payer, instructions, tuning),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => panic!(
                "{}: the single-shot confirm loop did not end within {LOOP_BOUND:?} (a regression in the give-up path)",
                running_test()
            ),
        }
    }

    /// A preflight rejection reaches a caller as `ErrorKind::RpcError(RpcResponseError { data:
    /// SendTransactionPreflightFailure(RpcSimulateTransactionResult { err: Some(..) }) })`, not as
    /// `ErrorKind::TransactionError` — the extractor must unwrap that shape too, or a program refusal
    /// under `skip_preflight: false` would classify as a transport failure.
    #[test]
    fn sender_error_transaction_error_unwraps_a_preflight_rejection() {
        use solana_client_v1::client_error::{ClientError, ClientErrorKind};
        use solana_client_v1::rpc_request::{RpcError, RpcResponseErrorData};
        use solana_client_v1::rpc_response::RpcSimulateTransactionResult;
        let sim: RpcSimulateTransactionResult =
            serde_json::from_str(r#"{"err":{"InstructionError":[0,{"Custom":1}]}}"#)
                .expect("a simulate result with only `err` set deserialises");
        let client_err = ClientError {
            request: None,
            kind: Box::new(ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32002,
                message: "Transaction simulation failed".to_string(),
                data: RpcResponseErrorData::SendTransactionPreflightFailure(sim),
            })),
        };
        let err = SenderError::Rpc {
            message: "preflight".to_string(),
            source: Box::new(client_err),
        };
        // Compared through `Debug` so this assertion reads the same whether the extractor hands back a
        // borrowed or an owned `TransactionError`.
        assert_eq!(
            format!("{:?}", sender_error_transaction_error(&err)),
            format!(
                "{:?}",
                Some(TransactionError::InstructionError(
                    0,
                    InstructionError::Custom(1)
                ))
            )
        );
    }

    // ===== the shared loaded-accounts formula =====

    #[test]
    fn required_loaded_accounts_bytes_sums_64_plus_len_per_account() {
        // Three accounts: one about-to-be-created (len 0), one small, one carrying real data.
        assert_eq!(
            required_loaded_accounts_bytes(&[0, 10, 1000]),
            64 + (64 + 10) + (64 + 1000)
        );
    }

    #[test]
    fn required_loaded_accounts_bytes_of_no_accounts_is_zero() {
        assert_eq!(required_loaded_accounts_bytes(&[]), 0);
    }

    // ===== `describe_rpc_error` must never let a request URL (or a secret in its
    // query string) reach a log line =====

    /// reqwest's own `Display` for a transport-level error appends `" for url (<URL>)"` — a minimal
    /// stand-in reproducing that exact shape (constructing a genuine reqwest transport error
    /// deterministically, without a live network attempt, is its own separate exercise; this crate depends
    /// on the STRING shape, not on any particular reqwest error variant) pins the actual redaction logic.
    struct UrlCarryingError;
    impl std::fmt::Display for UrlCarryingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "error sending request for url (https://x.example/?api-key=SECRET)"
            )
        }
    }

    /// **Mutation target:** returning `e.to_string()` unmodified (dropping the redaction entirely) turns
    /// this red — the raw string still contains `api-key` and `SECRET`.
    #[test]
    fn describe_rpc_error_strips_the_url_and_any_secret_it_carries() {
        let described = describe_rpc_error(&UrlCarryingError);
        assert!(
            !described.contains("api-key") && !described.contains("SECRET"),
            "the URL (and the secret in it) must not survive redaction, got: {described:?}"
        );
        assert!(
            described.contains("error sending request"),
            "the error KIND text must survive redaction, got: {described:?}"
        );
    }

    /// An error whose `Display` never mentions a URL at all must pass through unchanged — the redaction
    /// only ever strips from `" for url"` onward, never touches text that doesn't contain it.
    #[test]
    fn describe_rpc_error_passes_through_a_message_with_no_url_unchanged() {
        struct Plain;
        impl std::fmt::Display for Plain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection refused")
            }
        }
        assert_eq!(describe_rpc_error(&Plain), "connection refused");
    }

    /// `with_retry`'s own tests drive its exponential backoff under a
    /// paused virtual clock instead of real multi-second sleeps.
    #[tokio::test(start_paused = true)]
    async fn with_retry_counts_every_retry_before_succeeding() {
        let retries = AtomicU64::new(0);
        let attempts = Arc::new(AtomicU32::new(0));
        let result: Result<u32, solana_client_v1::client_error::ClientError> =
            with_retry(&retries, || {
                let attempts = attempts.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(solana_client_v1::client_error::ClientError::from(
                            solana_client_v1::client_error::ClientErrorKind::Custom(
                                "boom".to_string(),
                            ),
                        ))
                    } else {
                        Ok(42)
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(
            retries.load(Ordering::SeqCst),
            2,
            "2 failed attempts before the 3rd succeeds must count as exactly 2 retries"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_gives_up_after_max_attempts_counting_every_retry() {
        let retries = AtomicU64::new(0);
        let result: Result<(), solana_client_v1::client_error::ClientError> =
            with_retry(&retries, || async {
                Err(solana_client_v1::client_error::ClientError::from(
                    solana_client_v1::client_error::ClientErrorKind::Custom(
                        "always fails".to_string(),
                    ),
                ))
            })
            .await;
        assert!(
            result.is_err(),
            "must give up eventually, not retry forever"
        );
        assert_eq!(
            retries.load(Ordering::SeqCst),
            5,
            "6 total attempts means exactly 5 retries before giving up on the 6th"
        );
    }

    #[test]
    fn signature_status_chunks_never_exceed_the_rpc_nodes_per_call_cap() {
        let signatures: Vec<Signature> = (0..(MAX_SIGNATURE_STATUSES_PER_CALL * 3 + 7))
            .map(|_| Signature::new_unique())
            .collect();
        for chunk in signatures.chunks(MAX_SIGNATURE_STATUSES_PER_CALL) {
            assert!(chunk.len() <= MAX_SIGNATURE_STATUSES_PER_CALL);
        }
        let covered: usize = signatures
            .chunks(MAX_SIGNATURE_STATUSES_PER_CALL)
            .map(|c| c.len())
            .sum();
        assert_eq!(covered, signatures.len());
    }

    #[test]
    fn priority_fee_doubles_each_resubmit_up_to_the_cap() {
        assert_eq!(bumped_priority_fee(1_000, 200_000), 2_000);
        assert_eq!(bumped_priority_fee(2_000, 200_000), 4_000);
        assert_eq!(bumped_priority_fee(150_000, 200_000), 200_000);
        assert_eq!(bumped_priority_fee(200_000, 200_000), 200_000);
    }

    #[test]
    fn priority_fee_never_exceeds_the_configured_max_even_from_a_starting_value_above_it() {
        assert_eq!(bumped_priority_fee(500_000, 200_000), 200_000);
    }

    #[test]
    fn priority_lamports_ceils_and_matches_the_rome_sdk_v1_formula() {
        // Mirrors the Rome SDK's V1 batch test vectors exactly (module doc).
        assert_eq!(
            priority_lamports(1_400_000, 3).unwrap(),
            5,
            "ceil(1_400_000*3/1e6) == 5"
        );
        assert_eq!(
            priority_lamports(1_000_001, 1).unwrap(),
            2,
            "remainder case ceils up"
        );
        assert_eq!(priority_lamports(0, 1_000).unwrap(), 0);
    }

    #[tokio::test]
    async fn build_produces_a_v1_transaction_with_no_compute_budget_instructions() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let ix = solana_system_interface::instruction::transfer(
            &payer_legacy,
            &solana_program::pubkey::Pubkey::new_unique(),
            1,
        );
        let blockhash = Hash::default();
        let tx = build_tx(
            &payer,
            std::slice::from_ref(&ix),
            200_000,
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            1_000,
            blockhash,
        )
        .unwrap();
        let VersionedMessage::V1(msg) = &tx.message else {
            panic!("expected a V1 message");
        };
        // Exactly the caller's own instruction(s) — no ComputeBudget prefix (V1 carries limits in the
        // header config mask instead).
        assert_eq!(msg.instructions.len(), 1);
        assert_eq!(msg.config.compute_unit_limit, Some(200_000));
        assert_eq!(
            msg.config.loaded_accounts_data_size_limit,
            Some(rome_zk_batcher::config::default_loaded_accounts_data_size_limit())
        );
        assert!(msg.config.priority_fee.is_some());
    }

    #[tokio::test]
    async fn build_with_zero_priority_fee_leaves_the_config_field_unset() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let ix = solana_system_interface::instruction::transfer(
            &payer_legacy,
            &solana_program::pubkey::Pubkey::new_unique(),
            1,
        );
        let tx = build_tx(
            &payer,
            std::slice::from_ref(&ix),
            40_000,
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            0,
            Hash::default(),
        )
        .unwrap();
        let VersionedMessage::V1(msg) = &tx.message else {
            panic!("expected a V1 message");
        };
        assert_eq!(msg.config.priority_fee, None);
    }

    // ===== Every transaction shape this batcher sends
    // must be a real V1 (SIMD-0385) transaction — ≤ 4,096 B, with both limits in the header config mask.
    // Written against the real `zk_inbox_client` instruction builders (not a synthetic instruction), so
    // it proves the actual on-wire shape this binary sends, not merely that some V1 message compiles.
    #[test]
    fn design_frame_v1_tx_fits_4096_and_carries_both_header_limits() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let program_id = solana_program::pubkey::Pubkey::new_unique();
        let settlement_program = solana_program::pubkey::Pubkey::new_unique();
        let chain_id = 200_101u64;
        let batch = 7u64;
        let idx = 3u32;
        let body = vec![
            7u8;
            rome_zk_channel::FRAME_HEADER_LEN
                + rome_zk_channel::DEFAULT_MAX_FRAME_BODY_LEN
        ];
        let len = body.len() as u32;
        let body_hash = zk_inbox_client::chunk_body_hash(&body);
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                chain_id,
                batch,
                idx,
                len,
            ),
            zk_inbox_client::write_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                chain_id,
                batch,
                idx,
                0,
                body,
            ),
            zk_inbox_client::seal_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                chain_id,
                batch,
                idx,
                len,
                body_hash,
            ),
            zk_inbox_client::seal_leaf_ix(&program_id, &settlement_program, chain_id, batch, idx),
        ];
        let tx = build_tx(
            &payer,
            &ixs,
            40_000,
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            1_000,
            Hash::default(),
        )
        .expect("the design-max frame must compile into one V1 transaction");
        let bytes = wincode::serialize(&tx).expect("a signed V1 transaction always serializes");
        assert!(
            bytes.len() <= 4_096,
            "design frame V1 tx is {} bytes, over the 4,096-byte SIMD-0385 envelope",
            bytes.len()
        );
        // Merged V1 design frame, measured: Open + Write(whole body) + Seal{len,
        // body_hash} + SealLeaf = 4,084 B of the 4,096-B envelope — 12 B of headroom with the 32-B
        // body_hash included; the next lever if headroom is ever needed is dropping the priority-fee
        // field, never the body cap.
        assert_eq!(
            bytes.len(),
            4_084,
            "design-max V1 frame tx must be exactly 4,084 B (12 B headroom of 4,096); got {}",
            bytes.len()
        );
        assert_eq!(
            ixs.len(),
            4,
            "Open + one Write (whole body) + Seal + SealLeaf, no split"
        );
        match &tx.message {
            VersionedMessage::V1(m) => {
                assert_eq!(
                    m.instructions.len(),
                    4,
                    "no ComputeBudget instructions prepended"
                );
                assert_eq!(
                    m.config.compute_unit_limit,
                    Some(40_000),
                    "CU limit must be carried in the V1 header config mask, not a ComputeBudget ix"
                );
                assert_eq!(
                    m.config.loaded_accounts_data_size_limit,
                    Some(rome_zk_batcher::config::default_loaded_accounts_data_size_limit()),
                    "loaded-accounts-data-size limit must be carried in the V1 header config mask"
                );
            }
            other => panic!("expected VersionedMessage::V1, got {other:?}"),
        }
    }

    #[test]
    fn small_frame_v1_tx_fits_comfortably() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let program_id = solana_program::pubkey::Pubkey::new_unique();
        let settlement_program = solana_program::pubkey::Pubkey::new_unique();
        let body = vec![7u8; rome_zk_channel::FRAME_HEADER_LEN + 50];
        let len = body.len() as u32;
        let body_hash = zk_inbox_client::chunk_body_hash(&body);
        let ixs = vec![
            zk_inbox_client::open_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                1,
                1,
                0,
                len,
            ),
            zk_inbox_client::write_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                1,
                1,
                0,
                0,
                body,
            ),
            zk_inbox_client::seal_chunk_ix(
                &program_id,
                &payer_legacy,
                &settlement_program,
                1,
                1,
                0,
                len,
                body_hash,
            ),
            zk_inbox_client::seal_leaf_ix(&program_id, &settlement_program, 1, 1, 0),
        ];
        let tx = build_tx(
            &payer,
            &ixs,
            40_000,
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            1_000,
            Hash::default(),
        )
        .unwrap();
        let bytes = wincode::serialize(&tx).unwrap();
        assert!(bytes.len() <= 4_096);
    }

    #[test]
    fn open_batch_and_grow_900_v1_tx_fits_one_transaction() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let program_id = solana_program::pubkey::Pubkey::new_unique();
        let settlement_program = solana_program::pubkey::Pubkey::new_unique();
        let ixs = zk_inbox_client::open_and_grow_batch_ixs(
            &program_id,
            &payer_legacy,
            200_101,
            7,
            900,
            &settlement_program,
        );
        let tx = build_tx(
            &payer,
            &ixs,
            20_000,
            rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            1_000,
            Hash::default(),
        )
        .unwrap();
        let bytes = wincode::serialize(&tx).unwrap();
        assert!(
            bytes.len() <= 4_096,
            "OpenBatch+Grow(900) V1 tx is {} bytes",
            bytes.len()
        );
    }

    // ===== `compat::to_v1_instruction` pinning test. Lossless by
    // construction and fail-closed on flag errors (a real bug — is_writable hardcoded to `false` — would
    // make every writable account look read-only, changing the V1 header's readonly counts from the
    // legacy compile of the exact same instruction list), but until this test existed nothing pinned that
    // — that bug survived unnoticed. =====

    /// For one instruction list, asserts `compat::to_v1_instruction` reproduces every instruction
    /// field-for-field (program id, and per account meta: pubkey, is_signer, is_writable, in the same
    /// order; plus the raw instruction data) against the original list, and that compiling the
    /// converted list into a V1 message yields the *same* header counts
    /// (`num_required_signatures`/`num_readonly_signed_accounts`/`num_readonly_unsigned_accounts`) as
    /// compiling the untouched original list into a legacy message — the aggregate proof that no signer
    /// or writable flag silently flipped in the conversion.
    fn assert_v1_conversion_matches_legacy_compile(
        label: &str,
        ixs: &[Instruction],
        payer_legacy: &solana_program::pubkey::Pubkey,
        payer_v1: &solana_pubkey::Pubkey,
    ) {
        let v1_ixs: Vec<solana_instruction::Instruction> =
            ixs.iter().map(compat::to_v1_instruction).collect();

        assert_eq!(
            ixs.len(),
            v1_ixs.len(),
            "{label}: instruction count must be unchanged"
        );
        for (i, (legacy_ix, v1_ix)) in ixs.iter().zip(v1_ixs.iter()).enumerate() {
            assert_eq!(
                v1_ix.program_id,
                compat::to_v1_pubkey(&legacy_ix.program_id),
                "{label} ix {i}: program_id must round-trip"
            );
            assert_eq!(
                v1_ix.data, legacy_ix.data,
                "{label} ix {i}: instruction data must be byte-identical"
            );
            assert_eq!(
                v1_ix.accounts.len(),
                legacy_ix.accounts.len(),
                "{label} ix {i}: account count must be unchanged"
            );
            for (j, (legacy_meta, v1_meta)) in legacy_ix
                .accounts
                .iter()
                .zip(v1_ix.accounts.iter())
                .enumerate()
            {
                assert_eq!(
                    v1_meta.pubkey,
                    compat::to_v1_pubkey(&legacy_meta.pubkey),
                    "{label} ix {i} meta {j}: pubkey must round-trip"
                );
                assert_eq!(
                    v1_meta.is_signer, legacy_meta.is_signer,
                    "{label} ix {i} meta {j} ({}): is_signer must round-trip, not be hardcoded",
                    legacy_meta.pubkey
                );
                assert_eq!(
                    v1_meta.is_writable, legacy_meta.is_writable,
                    "{label} ix {i} meta {j} ({}): is_writable must round-trip, not be hardcoded",
                    legacy_meta.pubkey
                );
            }
        }

        // `solana_program::message` moved out entirely (API fallout) — the
        // legacy (non-versioned) `Message` now lives at `solana_message::legacy::Message`.
        let legacy_message = solana_message::legacy::Message::new_with_blockhash(
            ixs,
            Some(payer_legacy),
            &solana_program::hash::Hash::default(),
        );
        let config = v1::TransactionConfig::empty()
            .with_compute_unit_limit(0)
            .with_loaded_accounts_data_size_limit(0);
        let v1_message =
            v1::Message::try_compile_with_config(payer_v1, &v1_ixs, Hash::default(), config)
                .unwrap_or_else(|e| panic!("{label}: V1 compile failed: {e}"));

        assert_eq!(
            v1_message.header.num_required_signatures,
            legacy_message.header.num_required_signatures,
            "{label}: num_required_signatures must match the legacy compile"
        );
        assert_eq!(
            v1_message.header.num_readonly_signed_accounts,
            legacy_message.header.num_readonly_signed_accounts,
            "{label}: num_readonly_signed_accounts must match the legacy compile"
        );
        assert_eq!(
            v1_message.header.num_readonly_unsigned_accounts,
            legacy_message.header.num_readonly_unsigned_accounts,
            "{label}: num_readonly_unsigned_accounts must match the legacy compile"
        );
    }

    #[test]
    fn compat_to_v1_instruction_matches_legacy_compile_for_the_chunk_lane_plan() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let program_id = solana_program::pubkey::Pubkey::new_unique();
        let settlement_program = solana_program::pubkey::Pubkey::new_unique();
        let payload = vec![
            7u8;
            rome_zk_channel::FRAME_HEADER_LEN
                + rome_zk_channel::DEFAULT_MAX_FRAME_BODY_LEN
        ];
        let ixs = rome_zk_batcher::pipeline::plan_chunk(
            &program_id,
            &payer_legacy,
            &settlement_program,
            200_101,
            7,
            3,
            &payload,
        );
        assert_v1_conversion_matches_legacy_compile(
            "chunk-lane (Open+Write+Seal+SealLeaf)",
            &ixs,
            &payer_legacy,
            &payer.pubkey(),
        );
    }

    #[test]
    fn compat_to_v1_instruction_matches_legacy_compile_for_open_and_grow_batch() {
        let payer = Keypair::new();
        let payer_legacy = compat::from_v1_pubkey(&payer.pubkey());
        let program_id = solana_program::pubkey::Pubkey::new_unique();
        let settlement_program = solana_program::pubkey::Pubkey::new_unique();
        let ixs = zk_inbox_client::open_and_grow_batch_ixs(
            &program_id,
            &payer_legacy,
            200_101,
            7,
            900,
            &settlement_program,
        );
        assert_v1_conversion_matches_legacy_compile(
            "OpenBatch+Grow(900)",
            &ixs,
            &payer_legacy,
            &payer.pubkey(),
        );
    }

    // ===== scripted-fake harness for `run_send_and_confirm_many`, in the single-stage-per-frame shape. =====

    /// Every send this fake ever receives, in submission order: `(frame_index, stage_index, tx_index)`,
    /// tagged by encoding those three indices as the data of a harmless no-op instruction (see
    /// `marker_ix`) — the fake never needs real chunk/inbox instructions to exercise the sender's own
    /// ordering/bound/resubmit logic, only *something* it can tell apart.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct Marker {
        frame_index: usize,
        stage_index: usize,
        tx_index: usize,
    }

    /// `frame_index` gets a full 4 bytes — a fixture with more frames than
    /// `MAX_SIGNATURE_STATUSES_PER_CALL` (256) needs distinct frame indices past 255.
    fn marker_ix(frame_index: usize, stage_index: usize, tx_index: usize) -> Instruction {
        let mut data = Vec::with_capacity(6);
        data.extend_from_slice(&(frame_index as u32).to_le_bytes());
        data.push(stage_index as u8);
        data.push(tx_index as u8);
        Instruction {
            program_id: solana_system_interface::program::id(),
            accounts: vec![],
            data,
        }
    }

    fn decode_marker(tx: &VersionedTransaction) -> Marker {
        let VersionedMessage::V1(msg) = &tx.message else {
            panic!("fake only ever sees V1 messages")
        };
        // No ComputeBudget prefix under V1 — instruction 0 is this test's own marker directly.
        let ix = &msg.instructions[0];
        Marker {
            frame_index: u32::from_le_bytes(ix.data[0..4].try_into().unwrap()) as usize,
            stage_index: ix.data[4] as usize,
            tx_index: ix.data[5] as usize,
        }
    }

    /// One event in submission/polling order.
    #[derive(Debug, Clone)]
    enum Event {
        Sent(Marker),
        Polled,
    }

    /// A specific non-terminal shape a test wants the fake to hand back while a signature is still
    /// outstanding.
    #[derive(Debug, Clone, Copy)]
    enum PendingStatus {
        ConfirmationStatusProcessed,
        NoneStatusZeroConfirmations,
        /// Landed and voted on, not yet rooted: what real RPC reports between `confirmed` and `finalized`.
        ConfirmedNotFinalized,
    }

    #[derive(Default)]
    struct FakeState {
        block_height: u64,
        next_hash_seq: u64,
        sent_log: Vec<Marker>,
        events: Vec<Event>,
        released: HashMap<Signature, Option<TransactionError>>,
        auto_confirm: bool,
        status_poll_count: u64,
        chunk_sizes: Vec<usize>,
        /// Every V1 message's `config.compute_unit_limit` for every transaction ever sent, in order —
        /// proves every chunk-lane tx really carries the tuning's own
        /// `compute_unit_limit` end to end, not merely that *some* limit is set.
        sent_compute_unit_limits: Vec<Option<u32>>,
        /// Every V1 message's `config.loaded_accounts_data_size_limit` for every transaction ever sent, in
        /// order — `sent_compute_unit_limits` alone did not prove a
        /// *resubmit* still carries the tuning's `loaded_accounts_data_size_limit` (only
        /// `compute_unit_limit` was ever recorded), so the resubmit-site's own two tuning fields
        /// (`sender.rs`'s expired-submit `submit(...)` call) could silently drift without any test
        /// noticing.
        sent_loaded_accounts_data_size_limits: Vec<Option<u32>>,
        pending_status: HashMap<Signature, PendingStatus>,
        /// Signatures that read as not-yet-landed on their next poll and as `Ok` finalized after it — the
        /// root moving between two sequential `getSignatureStatuses` calls of one confirm pass.
        late_ok: std::collections::HashSet<Signature>,
        sent_signatures: HashMap<Marker, Vec<Signature>>,
        /// Applied once, right after the first status call returns: the block height jumps and every
        /// signature that call asked about takes this status — the chain moving between the status read
        /// and the block-height read of one confirm pass.
        after_first_poll: Option<(u64, PendingStatus)>,
    }

    #[derive(Clone)]
    struct FakeOps(Arc<Mutex<FakeState>>);

    impl FakeOps {
        fn new(auto_confirm: bool) -> Self {
            Self(Arc::new(Mutex::new(FakeState {
                auto_confirm,
                ..Default::default()
            })))
        }

        fn sent_log(&self) -> Vec<Marker> {
            self.0.lock().unwrap().sent_log.clone()
        }

        fn set_block_height(&self, h: u64) {
            self.0.lock().unwrap().block_height = h;
        }

        fn set_after_first_poll(&self, height: u64, kind: PendingStatus) {
            self.0.lock().unwrap().after_first_poll = Some((height, kind));
        }

        fn poll_count(&self) -> u64 {
            self.0.lock().unwrap().status_poll_count
        }

        fn chunk_sizes(&self) -> Vec<usize> {
            self.0.lock().unwrap().chunk_sizes.clone()
        }

        fn sent_compute_unit_limits(&self) -> Vec<Option<u32>> {
            self.0.lock().unwrap().sent_compute_unit_limits.clone()
        }

        fn sent_loaded_accounts_data_size_limits(&self) -> Vec<Option<u32>> {
            self.0
                .lock()
                .unwrap()
                .sent_loaded_accounts_data_size_limits
                .clone()
        }

        fn signatures_for(&self, marker: Marker) -> Vec<Signature> {
            self.0
                .lock()
                .unwrap()
                .sent_signatures
                .get(&marker)
                .cloned()
                .unwrap_or_default()
        }

        fn release(&self, sig: Signature, err: Option<TransactionError>) {
            let mut st = self.0.lock().unwrap();
            st.pending_status.remove(&sig);
            st.released.insert(sig, err);
        }

        fn set_ok_after_next_poll(&self, sig: Signature) {
            self.0.lock().unwrap().late_ok.insert(sig);
        }

        fn set_pending_status(&self, sig: Signature, kind: PendingStatus) {
            self.0.lock().unwrap().pending_status.insert(sig, kind);
        }

        /// Groups every `Sent` event into "waves" — the markers submitted between one poll boundary and
        /// the next.
        fn waves(&self) -> Vec<Vec<Marker>> {
            let events = self.0.lock().unwrap().events.clone();
            let mut waves = Vec::new();
            let mut current = Vec::new();
            for e in events {
                match e {
                    Event::Sent(m) => current.push(m),
                    Event::Polled => {
                        if !current.is_empty() {
                            waves.push(std::mem::take(&mut current));
                        }
                    }
                }
            }
            if !current.is_empty() {
                waves.push(current);
            }
            waves
        }
    }

    impl RpcOps for FakeOps {
        async fn send_transaction(
            &self,
            tx: &VersionedTransaction,
        ) -> Result<Signature, SenderError> {
            let marker = decode_marker(tx);
            let VersionedMessage::V1(msg) = &tx.message else {
                panic!("fake only ever sees V1 messages")
            };
            let compute_unit_limit = msg.config.compute_unit_limit;
            let loaded_accounts_data_size_limit = msg.config.loaded_accounts_data_size_limit;
            let signature = tx.signatures[0];
            let mut st = self.0.lock().unwrap();
            st.sent_log.push(marker);
            st.events.push(Event::Sent(marker));
            st.sent_compute_unit_limits.push(compute_unit_limit);
            st.sent_loaded_accounts_data_size_limits
                .push(loaded_accounts_data_size_limit);
            st.sent_signatures
                .entry(marker)
                .or_default()
                .push(signature);
            Ok(signature)
        }

        async fn get_signature_statuses(
            &self,
            signatures: &[Signature],
        ) -> Result<Vec<Option<TransactionStatus>>, SenderError> {
            let mut st = self.0.lock().unwrap();
            st.status_poll_count += 1;
            st.events.push(Event::Polled);
            st.chunk_sizes.push(signatures.len());
            let auto = st.auto_confirm;
            let out: Vec<Option<TransactionStatus>> = signatures
                .iter()
                .map(|s| {
                    if st.late_ok.remove(s) {
                        st.released.insert(*s, None);
                        return None;
                    }
                    if let Some(kind) = st.pending_status.get(s).copied() {
                        return Some(match kind {
                            PendingStatus::ConfirmationStatusProcessed => TransactionStatus {
                                slot: 0,
                                confirmations: Some(0),
                                status: Ok(()),
                                err: None,
                                confirmation_status: Some(
                                    solana_transaction_status_client_types_v1::TransactionConfirmationStatus::Processed,
                                ),
                            },
                            PendingStatus::ConfirmedNotFinalized => TransactionStatus {
                                slot: 0,
                                confirmations: Some(5),
                                status: Ok(()),
                                err: None,
                                confirmation_status: Some(
                                    solana_transaction_status_client_types_v1::TransactionConfirmationStatus::Confirmed,
                                ),
                            },
                            PendingStatus::NoneStatusZeroConfirmations => TransactionStatus {
                                slot: 0,
                                confirmations: Some(0),
                                status: Ok(()),
                                err: None,
                                confirmation_status: None,
                            },
                        });
                    }
                    if auto && !st.released.contains_key(s) {
                        st.released.insert(*s, None);
                    }
                    st.released.get(s).map(|outcome| TransactionStatus {
                        slot: 0,
                        confirmations: None,
                        status: outcome.clone().map_or(Ok(()), Err),
                        err: outcome.clone(),
                        confirmation_status: Some(
                            solana_transaction_status_client_types_v1::TransactionConfirmationStatus::Finalized,
                        ),
                    })
                })
                .collect();
            if let Some((height, kind)) = st.after_first_poll.take() {
                st.block_height = height;
                for s in signatures {
                    st.pending_status.insert(*s, kind);
                }
            }
            Ok(out)
        }

        async fn get_latest_blockhash(&self) -> Result<(Hash, u64), SenderError> {
            let mut st = self.0.lock().unwrap();
            st.next_hash_seq += 1;
            let mut bytes = [0u8; 32];
            bytes[0..8].copy_from_slice(&st.next_hash_seq.to_le_bytes());
            // A fresh blockhash is valid for the next 150 slots, matching real Solana's window.
            Ok((Hash::new_from_array(bytes), st.block_height + 150))
        }

        async fn get_block_height(&self) -> Result<u64, SenderError> {
            Ok(self.0.lock().unwrap().block_height)
        }
    }

    fn tuning() -> SendTuning {
        SendTuning {
            compute_unit_limit: 200_000,
            loaded_accounts_data_size_limit:
                rome_zk_batcher::config::default_loaded_accounts_data_size_limit(),
            priority_fee_micro_lamports: 1_000,
            max_priority_fee_micro_lamports: 200_000,
            confirm_timeout: Duration::from_secs(60),
            confirm_commitment: ConfirmCommitment::Finalized,
            status_poll_interval: Duration::from_millis(1),
        }
    }

    /// `Sent` means FINAL. Under Alpenglow `finalized` trails `confirmed` by 0-1 slot
    /// on devnet, so the default costs nothing there; under TowerBFT (mainnet today, ~31 slots) it is the
    /// safe default that the exit-prover's chain reads (at `finalized`) can never run ahead of.
    #[test]
    fn sender_default_commitment_is_finalized() {
        assert_eq!(ConfirmCommitment::default(), ConfirmCommitment::Finalized);
        assert_eq!(
            SendTuning::default().confirm_commitment,
            ConfirmCommitment::Finalized
        );
        assert!(SendTuning::default()
            .confirm_commitment
            .as_config()
            .is_finalized());
        assert!(ConfirmCommitment::Confirmed.as_config().is_confirmed());
    }

    /// 60 s was sized for a TowerBFT cluster; 15 s is the re-measured default. It is the unit of
    /// the overall give-up ceiling (ten times it), not a resubmit trigger.
    #[test]
    fn confirm_timeout_default_is_15s() {
        assert_eq!(
            SendTuning::default().confirm_timeout,
            Duration::from_secs(15)
        );
        assert_eq!(DEFAULT_CONFIRM_TIMEOUT_SECS, 15);
        assert_eq!(
            SendTuning::default().status_poll_interval,
            DEFAULT_STATUS_POLL_INTERVAL
        );
    }

    #[test]
    fn confirm_commitment_parses_finalized_and_confirmed_only() {
        assert_eq!(
            "finalized".parse::<ConfirmCommitment>().unwrap(),
            ConfirmCommitment::Finalized
        );
        assert_eq!(
            "confirmed".parse::<ConfirmCommitment>().unwrap(),
            ConfirmCommitment::Confirmed
        );
        assert!("processed".parse::<ConfirmCommitment>().is_err());
        assert!("Finalized ".parse::<ConfirmCommitment>().is_err());
    }

    /// The one predicate both the batched and the single-shot confirm loops use.
    #[test]
    fn landed_at_distinguishes_confirmed_from_finalized() {
        use solana_transaction_status_client_types_v1::TransactionConfirmationStatus as S;
        let st = |confirmations, confirmation_status| TransactionStatus {
            slot: 0,
            confirmations,
            status: Ok(()),
            err: None,
            confirmation_status,
        };
        let processed = st(Some(0), Some(S::Processed));
        let confirmed = st(Some(5), Some(S::Confirmed));
        let finalized = st(None, Some(S::Finalized));
        assert!(!landed_at(&processed, ConfirmCommitment::Confirmed));
        assert!(!landed_at(&processed, ConfirmCommitment::Finalized));
        assert!(landed_at(&confirmed, ConfirmCommitment::Confirmed));
        assert!(!landed_at(&confirmed, ConfirmCommitment::Finalized));
        assert!(landed_at(&finalized, ConfirmCommitment::Confirmed));
        assert!(landed_at(&finalized, ConfirmCommitment::Finalized));
    }

    /// Under the default (`finalized`) a transaction that has only reached `confirmed` is NOT done and is
    /// NOT resubmitted (the blockhash is still valid): the send is reported exactly once, when it is final.
    #[tokio::test]
    async fn a_send_confirmed_at_finalized_is_reported_sent_once() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning::default(),
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        while ops.sent_log().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let marker = Marker {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
        };
        let sig = ops.signatures_for(marker)[0];

        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        let target = ops.poll_count() + 5;
        for _ in 0..5_000 {
            if handle.is_finished() || ops.poll_count() >= target {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !handle.is_finished(),
            "a confirmed-but-not-finalized status must not complete a send at the default commitment"
        );

        ops.release(sig, None); // the fake's released status is a finalized one
        let outcome = handle
            .await
            .unwrap()
            .expect("must complete once the status is finalized");
        assert_eq!(outcome.total_steps, 1);
        assert_eq!(outcome.resubmits, 0, "no resubmit while waiting for final");
        assert_eq!(
            ops.sent_log().len(),
            1,
            "the transaction was sent exactly once"
        );
        assert_eq!(outcome.frames.len(), 1);
        assert_eq!(outcome.frames[0].signature, sig);
    }

    /// `confirmed` stays selectable: the same confirmed-not-finalized status completes the send.
    #[tokio::test]
    async fn a_send_confirmed_completes_when_confirmed_commitment_is_selected() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning {
                    confirm_commitment: ConfirmCommitment::Confirmed,
                    ..SendTuning::default()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        while ops.sent_log().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let sig = ops.signatures_for(Marker {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
        })[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        let outcome = handle.await.unwrap().expect("confirmed is enough here");
        assert_eq!(outcome.resubmits, 0);
        assert_eq!(ops.sent_log().len(), 1);
    }

    /// Every real frame is now a **single stage of a single transaction** — this is the
    /// shape `pipeline::plan_chunk` always produces.
    fn single_stage_frame(frame_index: usize) -> FramePlan {
        vec![vec![vec![marker_ix(frame_index, 0, 0)]]]
    }

    /// A signal-status shape that must never be treated as
    /// confirmed.
    #[tokio::test]
    async fn stage_never_advances_on_a_non_confirmed_status() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if !ops.sent_log().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let sig = ops.signatures_for(Marker {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
        })[0];

        ops.set_pending_status(sig, PendingStatus::ConfirmationStatusProcessed);
        let target = ops.poll_count() + 5;
        for _ in 0..5_000 {
            if handle.is_finished() || ops.poll_count() >= target {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !handle.is_finished(),
            "must not confirm on confirmation_status: Some(Processed)"
        );

        ops.set_pending_status(sig, PendingStatus::NoneStatusZeroConfirmations);
        let target2 = ops.poll_count() + 5;
        for _ in 0..5_000 {
            if handle.is_finished() || ops.poll_count() >= target2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !handle.is_finished(),
            "must not confirm on confirmation_status: None, confirmations: Some(0)"
        );

        ops.release(sig, None);
        handle
            .await
            .unwrap()
            .expect("must confirm once a real Confirmed status lands");
    }

    /// At most `in_flight` frames (one tx each) are ever concurrently
    /// outstanding.
    #[tokio::test]
    async fn at_most_in_flight_frames_are_ever_outstanding_at_once() {
        const IN_FLIGHT: usize = 3;
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frames: Vec<_> = (0..10).map(single_stage_frame).collect();
        run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            tuning(),
            IN_FLIGHT,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL,
        )
        .await
        .expect("all frames confirm under auto_confirm");

        let waves = ops.waves();
        for w in &waves {
            assert!(
                w.len() <= IN_FLIGHT,
                "wave {w:?} holds more than in_flight={IN_FLIGHT} transactions"
            );
        }
        assert!(
            waves.iter().any(|w| w.len() == IN_FLIGHT),
            "fixture must actually saturate in_flight at least once: {waves:?}"
        );
    }

    /// Every chunk-lane transaction the sender builds must actually
    /// carry the tuning's own `compute_unit_limit`, across every frame, in the V1 header config mask.
    #[tokio::test]
    async fn every_chunk_lane_tx_carries_the_configured_compute_unit_limit() {
        const CHUNK_CU: u32 = 40_000;
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frames: Vec<_> = (0..5).map(single_stage_frame).collect();
        run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            SendTuning {
                compute_unit_limit: CHUNK_CU,
                ..tuning()
            },
            10,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL,
        )
        .await
        .expect("all frames confirm under auto_confirm");

        let sent = ops.sent_compute_unit_limits();
        assert_eq!(
            sent.len(),
            5,
            "sanity: 5 single-tx frames must have sent 5 transactions"
        );
        for limit in &sent {
            assert_eq!(
                *limit,
                Some(CHUNK_CU),
                "every chunk-lane tx must carry compute_unit_limit in its V1 header config"
            );
        }
    }

    /// A failed transaction fails the frame at the right `(frame, stage, tx)` — the
    /// degenerate (0, 0, 0) case, but the same reporting path a multi-stage `OpenBatch`+`GrowBatch` plan
    /// still relies on.
    #[tokio::test]
    async fn a_failed_tx_fails_the_frame_at_the_right_frame_and_tx() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0), single_stage_frame(1)];

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                tuning(),
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if ops.sent_log().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let sig = ops.signatures_for(Marker {
            frame_index: 1,
            stage_index: 0,
            tx_index: 0,
        })[0];
        ops.release(sig, Some(TransactionError::AccountInUse));

        let err = handle.await.unwrap().expect_err("frame 1 must fail");
        match err {
            SenderError::StepFailed {
                frame_index,
                stage_index,
                tx_index,
                ..
            } => {
                assert_eq!(frame_index, 1);
                assert_eq!(stage_index, 0);
                assert_eq!(tx_index, 0);
            }
            other => panic!("expected StepFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_batch_size_1_chunks_3_outstanding_signatures_into_3_separate_calls() {
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frames: Vec<FramePlan> = (0..3).map(single_stage_frame).collect();
        run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            tuning(),
            10,
            Duration::from_millis(1),
            1,
        )
        .await
        .expect("all frames confirm under auto_confirm");
        assert_eq!(
            ops.chunk_sizes(),
            vec![1, 1, 1],
            "status_batch_size=1 must chunk the 3 outstanding signatures into 3 separate calls of 1 each"
        );
    }

    #[tokio::test]
    async fn a_status_batch_size_covering_every_outstanding_signature_makes_one_call() {
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frames: Vec<FramePlan> = (0..3).map(single_stage_frame).collect();
        run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            tuning(),
            10,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL,
        )
        .await
        .expect("all frames confirm under auto_confirm");
        assert_eq!(
            ops.chunk_sizes(),
            vec![3],
            "a status_batch_size covering every outstanding signature must ask for all 3 in one call"
        );
    }

    #[tokio::test]
    async fn status_batch_size_above_the_rpc_nodes_cap_is_clamped() {
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frame_count = MAX_SIGNATURE_STATUSES_PER_CALL + 4;
        let frames: Vec<FramePlan> = (0..frame_count).map(single_stage_frame).collect();
        run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            tuning(),
            frame_count + 1,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL * 10,
        )
        .await
        .expect("all frames confirm under auto_confirm");
        assert!(
            !ops.chunk_sizes().is_empty(),
            "fixture must actually exercise get_signature_statuses"
        );
        for size in ops.chunk_sizes() {
            assert!(
                size <= MAX_SIGNATURE_STATUSES_PER_CALL,
                "a get_signature_statuses call carried {size} signatures, over the RPC node's own cap of \
                 {MAX_SIGNATURE_STATUSES_PER_CALL}"
            );
        }
    }

    /// A resubmit re-sends the *same instructions* under a new signature.
    #[tokio::test]
    async fn resubmit_resends_the_same_step_with_a_new_signature() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        ops.set_block_height(0);

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if !ops.sent_log().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(ops.sent_log().len(), 1, "exactly one initial submission");

        ops.set_block_height(151);

        loop {
            if ops.sent_log().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let log = ops.sent_log();
        assert_eq!(log.len(), 2, "exactly one resubmit");
        assert_eq!(
            log[0], log[1],
            "a resubmit must target the same (frame, stage, tx) — same instructions, only the signature \
             (and priority fee / blockhash) differs"
        );
        // The original send and the resubmit must both carry the
        // tuning's own configured `compute_unit_limit` AND `loaded_accounts_data_size_limit` — a
        // `+1`-style slip at the resubmit call site (`run_send_and_confirm_many`'s expired-submit
        // `submit(...)` call) previously had nothing checking either field survived a resubmit.
        assert_eq!(
            ops.sent_compute_unit_limits(),
            vec![
                Some(tuning().compute_unit_limit),
                Some(tuning().compute_unit_limit)
            ],
            "both the original send and its resubmit must carry the configured compute_unit_limit"
        );
        assert_eq!(
            ops.sent_loaded_accounts_data_size_limits(),
            vec![
                Some(tuning().loaded_accounts_data_size_limit),
                Some(tuning().loaded_accounts_data_size_limit)
            ],
            "both the original send and its resubmit must carry the configured \
             loaded_accounts_data_size_limit"
        );

        ops.0.lock().unwrap().auto_confirm = true;
        handle
            .await
            .unwrap()
            .expect("must confirm once auto_confirm is on");
    }

    #[tokio::test]
    async fn resubmit_only_fires_once_block_height_passes_last_valid_block_height() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        ops.set_block_height(0);

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if !ops.sent_log().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(ops.sent_log().len(), 1, "exactly one initial submission");

        let lvbh = 150u64;
        let start_polls = ops.poll_count();
        loop {
            if ops.poll_count() >= start_polls + 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            ops.sent_log().len(),
            1,
            "no resubmit while block height is still below last_valid_block_height, however many polls"
        );

        ops.set_block_height(lvbh + 1);
        loop {
            if ops.sent_log().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            ops.sent_log().len(),
            2,
            "exactly one resubmit once block height passes last_valid_block_height"
        );

        let after_resubmit_polls = ops.poll_count();
        loop {
            if ops.poll_count() >= after_resubmit_polls + 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            ops.sent_log().len(),
            2,
            "no third send: the resubmit's own fresh last_valid_block_height is not yet passed"
        );

        ops.0.lock().unwrap().auto_confirm = true;
        handle
            .await
            .unwrap()
            .expect("must confirm once auto_confirm is on");
    }

    /// The *original* signature for a step lands `Ok` late, after a
    /// resubmit's duplicate signature has already come back `Err` — the step must still count as
    /// confirmed, not fail the batch.
    #[tokio::test]
    async fn a_late_confirming_original_signature_wins_over_a_resubmits_duplicate_error() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        ops.set_block_height(0);

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if !ops.sent_log().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ops.set_block_height(151);
        loop {
            if ops.sent_log().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let marker = Marker {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
        };
        let sigs = ops.signatures_for(marker);
        assert_eq!(sigs.len(), 2, "original + one resubmit");
        let (original_sig, resubmit_sig) = (sigs[0], sigs[1]);

        ops.release(resubmit_sig, Some(TransactionError::AccountInUse));
        ops.release(original_sig, None);

        let outcome = handle
            .await
            .unwrap()
            .expect("an Ok on the original signature must win over the resubmit's duplicate error");
        assert_eq!(outcome.frames.len(), 1);
        assert_eq!(outcome.frames[0].signature, original_sig);
    }

    fn marker0() -> Marker {
        Marker {
            frame_index: 0,
            stage_index: 0,
            tx_index: 0,
        }
    }

    /// Bound on every wait helper: a regression that stops the confirm loop early must fail by name, not
    /// hang until the CI job times out.
    const WAIT_BOUND: Duration = Duration::from_secs(5);

    async fn wait_until_sent(ops: &FakeOps, n: usize) {
        let waited = tokio::time::timeout(WAIT_BOUND, async {
            while ops.sent_log().len() < n {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "wait_until_sent: still {} sent after {WAIT_BOUND:?}, wanted {n} (the confirm loop ended or stalled early)",
            ops.sent_log().len()
        );
    }

    async fn wait_polls(ops: &FakeOps, n: u64) {
        let target = ops.poll_count() + n;
        let waited = tokio::time::timeout(WAIT_BOUND, async {
            while ops.poll_count() < target {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "wait_polls: only {} status polls after {WAIT_BOUND:?}, wanted {target} (the confirm loop ended early)",
            ops.poll_count()
        );
    }

    /// Batched path: a signature at `confirmed` or better is never resubmitted, even
    /// after its blockhash window has passed. It has landed; the only thing left is waiting for it to
    /// finalize, and a resubmit could only run the step again or fail it on chain.
    #[tokio::test]
    async fn a_confirmed_signature_is_not_resubmitted_when_its_blockhash_expires() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        ops.set_block_height(0);
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer2,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        ops.set_block_height(151); // past the 150-block window
        wait_polls(&ops, 10).await;
        assert_eq!(
            ops.sent_log().len(),
            1,
            "a confirmed signature must not be resubmitted when its blockhash expires"
        );
        ops.release(sig, None);
        let outcome = handle.await.unwrap().expect("finalizes");
        assert_eq!(outcome.resubmits, 0);
        assert_eq!(outcome.frames[0].signature, sig);
    }

    /// With the status pass split into sequential calls, the duplicate's failure can be
    /// read in one call and the original's `Ok` only appear after it. Before failing a slot, every one of
    /// its signatures is asked about again together; any `Ok` at the commitment wins.
    #[tokio::test]
    async fn a_slot_is_not_failed_until_all_its_signatures_are_asked_about_together() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        ops.set_block_height(0);
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer2,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                1, // one signature per status call, so a pass is several sequential calls
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        ops.set_block_height(151);
        wait_until_sent(&ops, 2).await;
        let sigs = ops.signatures_for(marker0());
        let (original, duplicate) = (sigs[0], sigs[1]);
        ops.release(duplicate, Some(TransactionError::AccountInUse));
        ops.set_ok_after_next_poll(original);
        let outcome = handle
            .await
            .unwrap()
            .expect("the original landed Ok; the duplicate's failure must not fail the frame");
        assert_eq!(outcome.frames[0].signature, original);
    }

    /// A single-shot send with a short `confirm_timeout` (ceiling 10x, hard backstop 20x it).
    fn spawn_single(
        ops: &FakeOps,
        confirm_timeout: Duration,
        poll: Duration,
    ) -> tokio::task::JoinHandle<Result<Signature, SenderError>> {
        let ops2 = ops.clone();
        let payer = Keypair::new();
        tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    confirm_timeout,
                    status_poll_interval: poll,
                    ..SendTuning::default()
                },
            )
            .await
        })
    }

    fn spawn_batch(
        ops: &FakeOps,
        confirm_timeout: Duration,
    ) -> tokio::task::JoinHandle<Result<BatchSendOutcome, SenderError>> {
        let ops2 = ops.clone();
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer,
                &frames,
                SendTuning {
                    confirm_timeout,
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        })
    }

    /// Single shot: a send that reached `confirmed` before the give-up ceiling and only
    /// finalizes after it is `Ok`, never `ConfirmTimeout`.
    #[tokio::test]
    async fn rv2_single_shot_confirmed_at_the_ceiling_is_not_reported_as_a_timeout() {
        let ops = FakeOps::new(false);
        let handle = spawn_single(&ops, Duration::from_millis(100), Duration::from_millis(1)); // ceiling 1 s, backstop 2 s
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        ops.set_block_height(151); // the attempt's window (150) is gone: only the landed guard keeps it waiting
        tokio::time::sleep(Duration::from_millis(1300)).await; // past the 1 s ceiling, inside the 2 s backstop
        assert!(
            !handle.is_finished(),
            "a landed (confirmed) send was given up on at the ceiling"
        );
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
        assert_eq!(ops.sent_log().len(), 1, "executed once");
    }

    /// Single shot: past the ceiling, an attempt whose blockhash is still valid can still
    /// land, so the send keeps waiting for it (without resubmitting).
    #[tokio::test]
    async fn rv2_single_shot_does_not_give_up_while_its_attempt_can_still_land() {
        let ops = FakeOps::new(false);
        let handle = spawn_single(&ops, Duration::from_millis(100), Duration::from_millis(1)); // ceiling 1 s
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_block_height(0); // lvbh is 150: the attempt is live
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            !handle.is_finished(),
            "gave up while the attempt was still live"
        );
        assert_eq!(ops.sent_log().len(), 1, "no resubmit past the ceiling");
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
    }

    /// Batched path: same two cases through `run_send_and_confirm_many`.
    #[tokio::test]
    async fn rv2_batched_confirmed_at_the_ceiling_is_not_reported_as_a_timeout() {
        let ops = FakeOps::new(false);
        let handle = spawn_batch(&ops, Duration::from_millis(100)); // ceiling 1 s, backstop 2 s
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        ops.set_block_height(151); // the attempt's window (150) is gone: only the landed guard keeps it waiting
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            !handle.is_finished(),
            "a landed (confirmed) frame was given up on at the ceiling"
        );
        ops.release(sig, None);
        let outcome = handle.await.unwrap().expect("finalizes after the ceiling");
        assert_eq!(outcome.frames[0].signature, sig);
        assert_eq!(outcome.resubmits, 0);
    }

    #[tokio::test]
    async fn rv2_batched_does_not_give_up_while_its_attempt_can_still_land() {
        let ops = FakeOps::new(false);
        let handle = spawn_batch(&ops, Duration::from_millis(100)); // ceiling 1 s, backstop 2 s
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            !handle.is_finished(),
            "gave up while the attempt was still live"
        );
        assert_eq!(ops.sent_log().len(), 1, "no resubmit past the ceiling");
        ops.release(sig, None);
        let outcome = handle.await.unwrap().expect("lands after the ceiling");
        assert_eq!(outcome.frames[0].signature, sig);
    }

    /// Past the ceiling nothing is resubmitted, and once the last attempt's blockhash has expired without
    /// landing the send fails at once (before the hard backstop): the error means nothing can still land.
    #[tokio::test]
    async fn rv2_past_the_ceiling_an_expired_unlanded_attempt_fails_without_a_resubmit() {
        let ops = FakeOps::new(false);
        let handle = spawn_single(&ops, Duration::from_millis(100), Duration::from_millis(1)); // ceiling 1 s
        wait_until_sent(&ops, 1).await;
        tokio::time::sleep(Duration::from_millis(1200)).await; // past the 1 s ceiling
        ops.set_block_height(151); // the attempt's window (150) is gone
        match handle.await.unwrap() {
            Err(SenderError::ConfirmTimeout {
                resubmits: 0,
                elapsed,
            }) => {
                assert!(
                    elapsed < Duration::from_millis(2000),
                    "failed only at the hard backstop ({elapsed:?}), not when the attempt expired"
                );
            }
            other => panic!("expected ConfirmTimeout with no resubmit, got {other:?}"),
        }
        assert_eq!(ops.sent_log().len(), 1, "no resubmit past the ceiling");
    }

    #[tokio::test]
    async fn rv2_batched_past_the_ceiling_an_expired_unlanded_attempt_fails_without_a_resubmit() {
        let ops = FakeOps::new(false);
        let handle = spawn_batch(&ops, Duration::from_millis(100)); // ceiling 1 s, backstop 2 s
        wait_until_sent(&ops, 1).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        ops.set_block_height(151);
        match handle.await.unwrap() {
            Err(SenderError::BatchConfirmTimeout {
                resubmits: 0,
                elapsed,
                unconfirmed: 1,
                total: 1,
            }) => assert!(elapsed < Duration::from_millis(2000), "{elapsed:?}"),
            other => panic!("expected BatchConfirmTimeout, got {other:?}"),
        }
        assert_eq!(ops.sent_log().len(), 1, "no resubmit past the ceiling");
    }

    /// The hard wall-clock backstop: a cluster whose block height has stalled (attempt "live" forever) still
    /// ends, at twice the ceiling.
    #[tokio::test]
    async fn rv2_a_stalled_cluster_hits_the_hard_backstop_at_twice_the_ceiling() {
        let ops = FakeOps::new(false);
        let handle = spawn_single(&ops, Duration::from_millis(20), Duration::from_millis(1)); // ceiling 200 ms
        wait_until_sent(&ops, 1).await;
        match handle.await.unwrap() {
            Err(SenderError::ConfirmTimeout { elapsed, .. }) => assert!(
                elapsed >= Duration::from_millis(400),
                "ended before the hard backstop (20x confirm_timeout): {elapsed:?}"
            ),
            other => panic!("expected ConfirmTimeout at the backstop, got {other:?}"),
        }
    }

    /// Raises the fake's block height by one every `every`, from `from`, until aborted: a slow cluster that
    /// is still advancing (50 ms per block here; 52 blocks in 2.6 s stays inside a 150-block window).
    fn advance_height(ops: &FakeOps, from: u64, every: Duration) -> tokio::task::JoinHandle<()> {
        let ops = ops.clone();
        tokio::spawn(async move {
            let mut h = from;
            loop {
                ops.set_block_height(h);
                h += 1;
                tokio::time::sleep(every).await;
            }
        })
    }

    /// Single shot: the backstop is a stall, not wall time. A cluster that is slow but
    /// still advancing must not fail a send whose attempt is still valid, however long it takes. (Wall-clock
    /// backstop: ceiling 1 s, backstop 2 s, so the old code failed it at 2 s.)
    #[tokio::test]
    async fn rv3_single_shot_a_slow_but_advancing_cluster_does_not_fail_a_live_send() {
        let ops = FakeOps::new(false);
        let ticker = advance_height(&ops, 0, Duration::from_millis(50));
        let handle = spawn_single(&ops, Duration::from_millis(100), Duration::from_millis(1));
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        tokio::time::sleep(Duration::from_millis(2700)).await; // past the 2 s wall-clock backstop
        let finished = handle.is_finished();
        ticker.abort();
        assert!(
            !finished,
            "a send on a still-advancing cluster was failed at the wall-clock backstop"
        );
        assert_eq!(ops.sent_log().len(), 1, "no resubmit past the ceiling");
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
    }

    /// Same, for a send that has landed (at `confirmed`, waiting for `finalized`) with an expired window.
    #[tokio::test]
    async fn rv3_single_shot_a_slow_but_advancing_cluster_does_not_fail_a_landed_send() {
        let ops = FakeOps::new(false);
        let ticker = advance_height(&ops, 151, Duration::from_millis(50));
        let handle = spawn_single(&ops, Duration::from_millis(100), Duration::from_millis(1));
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        tokio::time::sleep(Duration::from_millis(2700)).await;
        let finished = handle.is_finished();
        ticker.abort();
        assert!(
            !finished,
            "a landed send on a still-advancing cluster was failed at the wall-clock backstop"
        );
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
        assert_eq!(ops.sent_log().len(), 1, "executed once");
    }

    /// Same, batched, live attempt and landed attempt in one batch.
    #[tokio::test]
    async fn rv3_batched_a_slow_but_advancing_cluster_does_not_fail_live_or_landed_frames() {
        let ops = FakeOps::new(false);
        let ticker = advance_height(&ops, 0, Duration::from_millis(50));
        let ops2 = ops.clone();
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0), single_stage_frame(1)];
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_millis(100),
                    ..tuning()
                },
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        wait_until_sent(&ops, 2).await;
        let marker1 = Marker {
            frame_index: 1,
            stage_index: 0,
            tx_index: 0,
        };
        let (s0, s1) = (
            ops.signatures_for(marker0())[0],
            ops.signatures_for(marker1)[0],
        );
        ops.set_pending_status(s1, PendingStatus::ConfirmedNotFinalized);
        tokio::time::sleep(Duration::from_millis(2700)).await;
        let finished = handle.is_finished();
        ticker.abort();
        assert!(
            !finished,
            "the batch was failed at the wall-clock backstop on a still-advancing cluster"
        );
        ops.release(s0, None);
        ops.release(s1, None);
        let outcome = handle.await.unwrap().expect("both frames finalize");
        assert_eq!(outcome.resubmits, 0);
    }

    /// A landed send whose cluster has stalled past the backstop gets its own error, not
    /// "without confirmation": it executed and may still finalize.
    #[tokio::test]
    async fn rv3_single_shot_a_landed_send_at_the_stall_backstop_gets_its_own_error() {
        let ops = FakeOps::new(false);
        ops.set_block_height(151); // stalled
        let handle = spawn_single(&ops, Duration::from_millis(20), Duration::from_millis(1)); // backstop 400 ms
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        match handle.await.unwrap() {
            Err(e @ SenderError::LandedNotFinal { resubmits: 0, .. }) => {
                let text = e.to_string();
                assert!(
                    text.contains("landed") && !text.contains("without confirmation"),
                    "{text}"
                );
            }
            other => panic!("expected LandedNotFinal, got {other:?}"),
        }
        assert_eq!(ops.sent_log().len(), 1, "executed once");
    }

    #[tokio::test]
    async fn rv3_batched_a_landed_frame_at_the_stall_backstop_gets_its_own_error() {
        let ops = FakeOps::new(false);
        ops.set_block_height(151);
        let handle = spawn_batch(&ops, Duration::from_millis(20));
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        match handle.await.unwrap() {
            Err(
                e @ SenderError::BatchLandedNotFinal {
                    landed: 1,
                    unconfirmed: 1,
                    total: 1,
                    ..
                },
            ) => {
                let text = e.to_string();
                assert!(
                    text.contains("landed") && !text.contains("without confirmation"),
                    "{text}"
                );
            }
            other => panic!("expected BatchLandedNotFinal, got {other:?}"),
        }
    }

    /// Past the ceiling a batch does not START a queued frame for the first time. With
    /// one slot in flight, frame 0 is released after the 1 s ceiling; frame 1 must never be submitted and
    /// the batch ends with the timeout error counting it as unconfirmed.
    #[tokio::test]
    async fn rv3_past_the_ceiling_a_queued_frame_is_not_started() {
        let ops = FakeOps::new(false);
        let ops2 = ops.clone();
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0), single_stage_frame(1)];
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_millis(100), // ceiling 1 s, stall backstop 2 s
                    ..tuning()
                },
                1,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let s0 = ops.signatures_for(marker0())[0];
        tokio::time::sleep(Duration::from_millis(1300)).await; // past the ceiling, inside the backstop
        ops.release(s0, None);
        match handle.await.unwrap() {
            Err(SenderError::BatchConfirmTimeout {
                unconfirmed: 1,
                total: 2,
                ..
            }) => {}
            other => panic!(
                "expected BatchConfirmTimeout with the queued frame unconfirmed, got {other:?}"
            ),
        }
        assert_eq!(
            ops.sent_log().len(),
            1,
            "frame 1 was started past the ceiling"
        );
    }

    /// ... and a later STAGE of a frame is not started past the ceiling either.
    #[tokio::test]
    async fn rv3_past_the_ceiling_a_later_stage_is_not_started() {
        let ops = FakeOps::new(false);
        let ops2 = ops.clone();
        let payer = Keypair::new();
        let frames: Vec<FramePlan> = vec![vec![
            vec![vec![marker_ix(0, 0, 0)]],
            vec![vec![marker_ix(0, 1, 0)]],
        ]];
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer,
                &frames,
                SendTuning {
                    confirm_timeout: Duration::from_millis(100),
                    ..tuning()
                },
                1,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let s0 = ops.signatures_for(marker0())[0];
        tokio::time::sleep(Duration::from_millis(1300)).await;
        ops.release(s0, None);
        match handle.await.unwrap() {
            Err(SenderError::BatchConfirmTimeout {
                unconfirmed: 1,
                total: 1,
                ..
            }) => {}
            other => {
                panic!("expected BatchConfirmTimeout with the frame unconfirmed, got {other:?}")
            }
        }
        assert_eq!(
            ops.sent_log().len(),
            1,
            "stage 1 was started past the ceiling"
        );
    }

    /// The block height is read before the status pass, so a transaction that lands
    /// between the two reads is never resubmitted. Here the first status read says "not landed" and the
    /// chain then moves (height 151, signature confirmed): read in the wrong order, that pass would see the
    /// new height with the old status and resubmit a transaction that has landed.
    #[tokio::test]
    async fn rv2_height_is_read_before_the_status_pass_so_a_landing_between_them_is_not_resubmitted(
    ) {
        let ops = FakeOps::new(false);
        ops.set_after_first_poll(151, PendingStatus::ConfirmedNotFinalized);
        let handle = spawn_single(&ops, Duration::from_secs(60), Duration::from_millis(1));
        wait_until_sent(&ops, 1).await;
        wait_polls(&ops, 10).await;
        assert_eq!(
            ops.sent_log().len(),
            1,
            "resubmitted a transaction that landed between the status read and the height read"
        );
        let sig = ops.signatures_for(marker0())[0];
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
    }

    /// The single-shot poll interval is `tuning.status_poll_interval`, not a constant.
    #[tokio::test]
    async fn rv2_single_shot_poll_count_follows_status_poll_interval() {
        let ops = FakeOps::new(false);
        let handle = spawn_single(&ops, Duration::from_secs(60), Duration::from_millis(100));
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        let before = ops.poll_count();
        tokio::time::sleep(Duration::from_millis(650)).await;
        let polls = ops.poll_count() - before;
        assert!(
            (3..=9).contains(&polls),
            "a 100 ms interval over 650 ms should poll about 6 times, got {polls}"
        );
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
    }

    /// The single-shot path, driven by the fake. Default tuning is `finalized`: a send that has only
    /// reached `confirmed` is not done.
    #[tokio::test]
    async fn single_shot_at_the_default_commitment_waits_for_finalized() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer2,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    status_poll_interval: Duration::from_millis(1),
                    ..SendTuning::default()
                },
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        wait_polls(&ops, 5).await;
        assert!(
            !handle.is_finished(),
            "confirmed is not enough at the default commitment"
        );
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
    }

    /// ... and `confirmed` stays selectable on the single-shot path.
    #[tokio::test]
    async fn single_shot_completes_at_confirmed_when_selected() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer2,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    confirm_commitment: ConfirmCommitment::Confirmed,
                    status_poll_interval: Duration::from_millis(1),
                    ..SendTuning::default()
                },
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
        assert_eq!(ops.sent_log().len(), 1);
    }

    /// A status at `confirmed` that stays unfinalized well past `confirm_timeout` causes
    /// NO resubmit, and the send is reported `Ok` exactly once when it finalizes. (The old single-shot loop
    /// resubmitted on the wall clock and forgot the first signature.)
    #[tokio::test]
    async fn single_shot_confirmed_past_the_timeout_is_not_resubmitted_and_is_reported_ok_once() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer2,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    confirm_timeout: Duration::from_millis(100), // overall ceiling is 10x this
                    status_poll_interval: Duration::from_millis(1),
                    ..SendTuning::default()
                },
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        let sig = ops.signatures_for(marker0())[0];
        ops.set_pending_status(sig, PendingStatus::ConfirmedNotFinalized);
        ops.set_block_height(151); // past the attempt's window: only the landed guard stops a resubmit
        tokio::time::sleep(Duration::from_millis(250)).await; // well past confirm_timeout
        assert_eq!(
            ops.sent_log().len(),
            1,
            "no resubmit of a send that has already confirmed"
        );
        ops.release(sig, None);
        assert_eq!(handle.await.unwrap().unwrap(), sig);
        assert_eq!(ops.sent_log().len(), 1, "executed once");
    }

    /// The original lands after a resubmit; the result is `Ok` with the ORIGINAL's
    /// signature, not `StepFailed` from the duplicate.
    #[tokio::test]
    async fn single_shot_original_landing_after_a_resubmit_is_ok_with_the_originals_signature() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        ops.set_block_height(0);
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer2,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    confirm_timeout: Duration::from_secs(600),
                    status_poll_interval: Duration::from_millis(1),
                    ..tuning()
                },
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        ops.set_block_height(151);
        wait_until_sent(&ops, 2).await;
        let sigs = ops.signatures_for(marker0());
        assert_eq!(sigs.len(), 2);
        ops.release(sigs[1], Some(TransactionError::AccountInUse));
        ops.release(sigs[0], None);
        assert_eq!(handle.await.unwrap().unwrap(), sigs[0]);
    }

    /// An on-chain failure is still reported as `StepFailed` at (0, 0, 0), and a wall-clock give-up as
    /// `ConfirmTimeout`: the shapes the single-shot callers match on.
    #[tokio::test]
    async fn single_shot_keeps_its_error_shapes() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let ops2 = ops.clone();
        let payer2 = payer.insecure_clone();
        let failing = tokio::spawn(async move {
            run_send_and_confirm_one(
                &ops2,
                &payer2,
                &[marker_ix(0, 0, 0)],
                SendTuning {
                    status_poll_interval: Duration::from_millis(1),
                    ..SendTuning::default()
                },
            )
            .await
        });
        wait_until_sent(&ops, 1).await;
        ops.release(
            ops.signatures_for(marker0())[0],
            Some(TransactionError::AccountInUse),
        );
        match failing.await.unwrap() {
            Err(SenderError::StepFailed {
                frame_index: 0,
                stage_index: 0,
                tx_index: 0,
                err: TransactionError::AccountInUse,
            }) => {}
            other => panic!("expected StepFailed at (0,0,0), got {other:?}"),
        }

        let ops = FakeOps::new(false); // never lands
        let r = run_send_and_confirm_one(
            &ops,
            &payer,
            &[marker_ix(0, 0, 0)],
            SendTuning {
                confirm_timeout: Duration::from_millis(5),
                status_poll_interval: Duration::from_millis(1),
                ..SendTuning::default()
            },
        )
        .await;
        assert!(
            matches!(r, Err(SenderError::ConfirmTimeout { .. })),
            "got {r:?}"
        );
    }

    /// `stage_latencies` has exactly one entry for a single-stage frame, equal to
    /// `confirm_latency`.
    #[tokio::test]
    async fn stage_latencies_has_exactly_one_entry_for_a_single_stage_frame() {
        let ops = FakeOps::new(true);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0)];
        let outcome = run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            tuning(),
            10,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL,
        )
        .await
        .expect("all stages confirm under auto_confirm");

        assert_eq!(outcome.frames.len(), 1);
        let frame = &outcome.frames[0];
        assert_eq!(frame.stage_latencies.len(), 1);
        // Both are measured from the same start (`first_submitted`) but at two separately-recorded
        // instants a beat apart (`stage_confirmed_at` is captured first; `confirm_latency` calls
        // `.elapsed()` again afterwards) — equal in substance (single-stage frame, one hop), not bit-for-
        // bit identical wall-clock reads.
        assert!(
            frame.stage_latencies[0] <= frame.confirm_latency,
            "a single stage's own latency cannot exceed the frame's total latency: {:?} > {:?}",
            frame.stage_latencies[0],
            frame.confirm_latency
        );
        assert!(
            frame.confirm_latency - frame.stage_latencies[0] < Duration::from_millis(50),
            "single-stage frame: stage and frame latency must be within a beat of each other"
        );
    }

    #[tokio::test]
    async fn a_step_error_surfaces_as_step_failed_with_the_right_frame_and_step() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames = vec![single_stage_frame(0), single_stage_frame(1)];

        let ops2 = ops.clone();
        let payer_for_task = payer.insecure_clone();
        let handle = tokio::spawn(async move {
            run_send_and_confirm_many(
                &ops2,
                &payer_for_task,
                &frames,
                tuning(),
                4,
                Duration::from_millis(1),
                MAX_SIGNATURE_STATUSES_PER_CALL,
            )
            .await
        });

        loop {
            if ops.sent_log().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let sig = ops.signatures_for(Marker {
            frame_index: 1,
            stage_index: 0,
            tx_index: 0,
        })[0];
        ops.release(sig, Some(TransactionError::AccountInUse));

        let err = handle.await.unwrap().expect_err("frame 1 must fail");
        match err {
            SenderError::StepFailed {
                frame_index,
                stage_index,
                tx_index,
                ..
            } => {
                assert_eq!(frame_index, 1);
                assert_eq!(stage_index, 0);
                assert_eq!(tx_index, 0);
            }
            other => panic!("expected StepFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_confirm_timeout_unconfirmed_counts_outstanding_plus_queued() {
        let ops = FakeOps::new(false);
        let payer = Keypair::new();
        let frames: Vec<_> = (0..5).map(single_stage_frame).collect();
        let err = run_send_and_confirm_many(
            &ops,
            &payer,
            &frames,
            SendTuning {
                confirm_timeout: Duration::from_millis(5),
                ..tuning()
            },
            2,
            Duration::from_millis(1),
            MAX_SIGNATURE_STATUSES_PER_CALL,
        )
        .await
        .expect_err("nothing ever confirms, so this must time out");
        match err {
            SenderError::BatchConfirmTimeout {
                unconfirmed, total, ..
            } => {
                assert_eq!(total, 5);
                assert_eq!(
                    unconfirmed, 5,
                    "2 outstanding (in `slots`) + 3 still queued must both count"
                );
            }
            other => panic!("expected BatchConfirmTimeout, got {other:?}"),
        }
    }

    /// `with_retry`'s warn line Debug-formatted `e.kind()`, and reqwest's `Debug` carries
    /// the full request URL (api-key query string included) — the one RPC-error log line `describe_rpc_error`
    /// did not reach. The retry line now goes through one redacting helper, never `?e`.
    #[test]
    fn the_retry_warning_never_carries_the_rpc_url() {
        let e = solana_client_v1::client_error::ClientError::from(
            solana_client_v1::client_error::ClientErrorKind::Custom(
                "error sending request for url (https://x.example/?api-key=SECRET)".to_string(),
            ),
        );
        let msg = describe_retry_failure(3, &e);
        assert!(msg.contains("attempt 3"), "got: {msg}");
        assert!(
            !msg.contains("api-key") && !msg.contains("SECRET") && !msg.contains("x.example"),
            "the URL and its secret must not survive: {msg}"
        );
    }
}
