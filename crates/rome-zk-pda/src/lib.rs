//! `create_or_adopt_pda`: the single place both `zk-inbox` and `zk-settlement` bring a PDA-owned account into
//! existence, closing a griefing class found at **every** creation site ("no instruction may be blocked by lamports
//! sent to a predictable PDA").
//!
//! ## The attack
//! A PDA's address is `find_program_address`-derived from public inputs (chain id, batch id, …) — anyone
//! can compute it and `system_instruction::transfer` the rent-exempt minimum to it *before* the
//! instruction that means to create it ever lands. `system_instruction::create_account`'s own CPI then
//! fails `AccountAlreadyInUse` (`solana-system-program` `system_processor.rs`: it refuses to touch a
//! destination that already holds lamports), and — for an instruction whose own state (a sequential
//! cursor, an in-order finality head) can never route around that one id — this turns a one-time
//! griefing cost into a **permanent** liveness kill, not merely recoverable griefing.
//!
//! ## The fix
//! If the PDA holds no lamports, `create_account` exactly as before. If it already holds some (the
//! griefing case — nobody but this program's own `invoke_signed` can ever make the PDA itself sign, so
//! lamports are the *only* thing an outsider can have put there), adopt it in place instead: top up
//! whatever rent is still missing, `allocate` the space, then `assign` it to `owner` — both via
//! `invoke_signed` with the PDA's own seeds, exactly as `create_account` itself would need. Either path
//! ends with `pda` owned by `owner`, sized exactly `space`, ready for the caller to write its header into
//! — the caller cannot tell after the fact which path ran (donated lamports simply become part of the
//! account's balance).
//!
//! `create_or_adopt_pda` does **not** decide whether "already owned by `owner`, with real data" should be
//! an error (a true "already initialised") — that is instruction-specific (e.g. `InitBatchCursor` must
//! still refuse a second call for the same chain). Callers that need that guarantee check it themselves,
//! by owner + `data_len() != 0`, *before* calling this helper — never by relying on `create_account`'s own
//! error, which no longer distinguishes "already initialised" from "merely griefed".

// `system_instruction`/`system_program` are no longer re-exported from `solana_program`'s root in the Agave 4.x
// line (they moved to a dedicated crate, `solana_program`'s own doc: "see
// solana_system_interface::program::check_id"); aliased on import so every call site below
// (`system_instruction::create_account`, `system_program::id()`, …) is unchanged.
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use solana_system_interface::{instruction as system_instruction, program as system_program};

/// Lamports still needed to reach `rent_exempt_minimum`, given `have` already sits in the account.
/// Pure and independent of any Solana runtime — trivially unit-testable off-chain.
fn rent_shortfall(rent_exempt_minimum: u64, have: u64) -> u64 {
    rent_exempt_minimum.saturating_sub(have)
}

/// Creates `pda` at `space` bytes owned by `owner` — or, if it is already system-owned but pre-funded
/// with lamports (the griefing case above), adopts it in place instead. `seeds` must be exactly the
/// PDA's own signer seeds, including the bump — the same seeds a bare `create_account` CPI would need.
///
/// Callers are responsible for any "already truly initialised" check they need *before* calling this
/// (see the module doc) — this function only ever distinguishes "no lamports yet" (create) from "some
/// lamports already there" (adopt); it does not itself inspect `pda.owner`/`pda.data_len()`.
pub fn create_or_adopt_pda<'a>(
    payer: &AccountInfo<'a>,
    pda: &AccountInfo<'a>,
    sys: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    seeds: &[&[u8]],
) -> ProgramResult {
    if pda.lamports() == 0 {
        return invoke_signed(
            &system_instruction::create_account(
                payer.key,
                pda.key,
                Rent::get()?.minimum_balance(space),
                space as u64,
                owner,
            ),
            &[payer.clone(), pda.clone(), sys.clone()],
            &[seeds],
        );
    }
    adopt_funded_pda(payer, pda, sys, owner, space, seeds)
}

/// The adopt path: top up rent (a plain `transfer` — `payer` is already a real transaction signer, so
/// this CPI needs no PDA signature), then `allocate` + `assign` (both require the *destination* account to
/// sign, which only `invoke_signed` with the PDA's own seeds can provide for an off-curve address nobody
/// holds a private key for).
fn adopt_funded_pda<'a>(
    payer: &AccountInfo<'a>,
    pda: &AccountInfo<'a>,
    sys: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    seeds: &[&[u8]],
) -> ProgramResult {
    if *pda.owner != system_program::id() || pda.data_len() != 0 {
        // Not the griefing shape this helper adopts (system-owned, no data, just donated lamports) — a
        // caller that needed to rule out "already truly created" must have done so before calling this
        // helper (see the module doc); reaching here with neither shape true is a caller bug, not a
        // griefing attempt this function can safely paper over.
        return Err(ProgramError::InvalidAccountData);
    }
    let shortfall = rent_shortfall(Rent::get()?.minimum_balance(space), pda.lamports());
    if shortfall > 0 {
        invoke(
            &system_instruction::transfer(payer.key, pda.key, shortfall),
            &[payer.clone(), pda.clone(), sys.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(pda.key, space as u64),
        &[pda.clone(), sys.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(pda.key, owner),
        &[pda.clone(), sys.clone()],
        &[seeds],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rent_shortfall_is_zero_once_already_covered() {
        assert_eq!(rent_shortfall(1_000, 1_000), 0);
        assert_eq!(rent_shortfall(1_000, 1_500), 0, "must not go negative");
    }

    #[test]
    fn rent_shortfall_is_the_remaining_gap() {
        assert_eq!(rent_shortfall(1_000, 300), 700);
        assert_eq!(rent_shortfall(1_000, 0), 1_000);
    }
}
