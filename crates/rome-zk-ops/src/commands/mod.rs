//! One module per command. Each is an `async fn` over [`crate::chain::Chain`] that returns a [`Report`] or a
//! named [`OpsError`], and never prints: the caller prints.

pub mod bridge;
pub mod chain_id;
pub mod chain_status;
pub mod exit_config;
pub mod init_cursor;
pub mod migrate;
pub mod pdas;
pub mod refund_deposit;
pub mod register;

use crate::chain::Chain;
use crate::error::{Mode, OpsError, Report};
use crate::keys::Signers;
use solana_program::{instruction::Instruction, pubkey::Pubkey};

/// A dry run prints the signed transaction and stops. A confirmed run sends it and records the signature.
pub(crate) async fn execute<C: Chain>(
    chain: &C,
    mode: Mode,
    label: &str,
    ix: Instruction,
    signers: &Signers,
    report: &mut Report,
) -> Result<Option<String>, OpsError> {
    match mode {
        Mode::Dry => {
            report
                .lines
                .extend(crate::render::dry_run(label, &ix, signers)?);
            Ok(None)
        }
        Mode::Confirm => {
            let sig = chain
                .send(std::slice::from_ref(&ix), signers)
                .await
                .map_err(|e| OpsError::chain("SendFailed", format!("{label}: {e}")))?;
            report.signature = Some(sig.clone());
            Ok(Some(sig))
        }
    }
}

/// Like [`execute`], for a transaction that carries several instructions (and none of the settlement program's, so
/// the dry run does not try to decode them as settlement instructions).
pub(crate) async fn execute_many<C: Chain>(
    chain: &C,
    mode: Mode,
    label: &str,
    ixs: &[Instruction],
    signers: &Signers,
    report: &mut Report,
) -> Result<Option<String>, OpsError> {
    match mode {
        Mode::Dry => {
            report
                .lines
                .extend(crate::render::dry_run_many(label, ixs, signers)?);
            Ok(None)
        }
        Mode::Confirm => {
            let sig = chain
                .send(ixs, signers)
                .await
                .map_err(|e| OpsError::chain("SendFailed", format!("{label}: {e}")))?;
            report.signature = Some(sig.clone());
            Ok(Some(sig))
        }
    }
}

/// Reads an account the command needs. A confirmed run treats a failed read as a refusal named `name`; a dry run
/// notes it in the report and carries on without the check that needed it (`Ok(None)` is then ambiguous with a
/// missing account, so the caller gets `Read::Unavailable` instead).
pub(crate) enum Read {
    Missing,
    Found(Vec<u8>),
    Unavailable,
}

pub(crate) async fn read<C: Chain>(
    chain: &C,
    mode: Mode,
    key: &Pubkey,
    what: &str,
    name: &'static str,
    report: &mut Report,
) -> Result<Read, OpsError> {
    match chain.account(key).await {
        Ok(Some(d)) => Ok(Read::Found(d)),
        Ok(None) => Ok(Read::Missing),
        Err(e) => {
            let detail = format!(
                "the RPC request failed, so {what} could not be read (this is not a missing account); the client said: {e}"
            );
            if mode.is_confirm() {
                Err(OpsError::chain(name, detail))
            } else {
                report.line(format!(
                    "(dry run: {name}: {detail}; the checks that need it are skipped)"
                ));
                Ok(Read::Unavailable)
            }
        }
    }
}

pub(crate) async fn read_slot<C: Chain>(
    chain: &C,
    mode: Mode,
    report: &mut Report,
) -> Result<Option<u64>, OpsError> {
    match chain.slot().await {
        Ok(s) => Ok(Some(s)),
        Err(e) => {
            let detail = format!("the current slot could not be read; the client said: {e}");
            if mode.is_confirm() {
                Err(OpsError::chain("SlotLookupFailed", detail))
            } else {
                report.line(format!("(dry run: SlotLookupFailed: {detail})"));
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fake;
