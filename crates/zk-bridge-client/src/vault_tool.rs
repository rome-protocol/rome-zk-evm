//! The `vault` example's planning logic, lifted out of the binary so the ORDER of its guards is pinned by tests
//! over fakes (with the logic in the example, deleting a check would change nothing the tests see, and the tool's
//! `VaultSettlementMismatch` guard would be decorative). Same `Deps`-over-fakes shape
//! `rome-zk-exit-prover::core`/`run` use: the example wires an RPC client into [`VaultChain`] and a real send into
//! [`execute`]; every decision lives here, RPC-free.
//!
//! Contract (operators rely on every line):
//! - [`plan_fund`] REFUSES `SettlementMismatch` (via [`crate::check_vault_settlement`]) BEFORE any instruction is
//!   built — `vault_config`'s address is a PDA derived from the caller-supplied settlement program, and
//!   `programs/zk-bridge::fund` never re-checks that derivation on chain, so this client check is the ONLY guard.
//! - [`Mode::from_args`]: dry-run is the DEFAULT; only a literal `--confirm` sends.
//! - [`execute`] calls `send` exactly once in [`Mode::Confirm`] and never in [`Mode::DryRun`].

use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::{DecodeError, VaultConfigAccount, VaultSettlementMismatch};

/// The one chain read the vault tool needs: the raw data of an account, `Ok(None)` when it does not
/// exist, `Err` only when the read itself failed (never conflated — the same distinction
/// `rome-zk-prover`'s `RpcFetch::get_account` draws).
pub trait VaultChain {
    fn account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String>;
}

/// Every way [`plan_fund`]/[`plan_init_vault`] refuse BEFORE an instruction exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The chain read itself failed (unreachable RPC, timeout) — named, in EITHER mode.
    VaultConfigFetchFailed { pda: Pubkey, error: String },
    /// `fund` against a vault that was never initialised.
    VaultConfigNotFound { pda: Pubkey },
    /// The account at the derived address is not a decodable `vault_config`.
    VaultConfigUndecodable { pda: Pubkey, error: String },
    /// The vault this address resolves to records a DIFFERENT settlement program.
    VaultSettlementMismatch(VaultSettlementMismatch),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::VaultConfigFetchFailed { pda, error } => {
                write!(
                    f,
                    "VaultConfigFetchFailed: could not fetch vault_config {pda}: {error}"
                )
            }
            Refusal::VaultConfigNotFound { pda } => write!(
                f,
                "VaultConfigNotFound: vault_config {pda} does not exist — run init-vault first"
            ),
            Refusal::VaultConfigUndecodable { pda, error } => {
                write!(f, "VaultConfigUndecodable: vault_config {pda}: {error}")
            }
            Refusal::VaultSettlementMismatch(e) => write!(f, "{e}"),
        }
    }
}

/// A `fund` ready to inspect or send — only ever produced AFTER every refusal above has been ruled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundPlan {
    pub vault_config_pda: Pubkey,
    pub cfg: VaultConfigAccount,
    pub funder_token_account: Pubkey,
    pub ix: Instruction,
}

/// An `init-vault` decision: the vault already exists (nothing to send), or the instruction to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitVaultPlan {
    AlreadyInitialized {
        vault_config_pda: Pubkey,
        cfg: VaultConfigAccount,
    },
    Create {
        vault_config_pda: Pubkey,
        ix: Instruction,
    },
}

/// What `show-vault` reports — every decision (exists? decodable? token account present?) made here, by
/// name, never inline in the example.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultView {
    pub vault_config_pda: Pubkey,
    pub cfg: VaultConfigAccount,
    pub vault_authority: Pubkey,
    pub vault_token: Pubkey,
    /// `None` when the vault token account does not exist (InitVault should have created it).
    pub vault_token_balance: Option<u64>,
}

/// Reads `vault_config` and, when it exists, the vault's own token account: `Ok(None)` = not initialised.
pub fn describe_vault<C: VaultChain>(
    chain: &C,
    bridge: &Pubkey,
    settlement: &Pubkey,
    chain_id: u64,
) -> Result<Option<VaultView>, Refusal> {
    let Some((vault_config_pda, cfg)) = read_vault_config(chain, bridge, settlement, chain_id)?
    else {
        return Ok(None);
    };
    let (vault_token, _) = crate::vault_token_pda(bridge, settlement, chain_id, &cfg.mint);
    let (vault_authority, _) = crate::vault_authority_pda(bridge, settlement, chain_id);
    let vault_token_balance = chain
        .account_data(&vault_token)
        .map_err(|error| Refusal::VaultConfigFetchFailed {
            pda: vault_token,
            error,
        })?
        .map(|data| zk_bridge::token::read_token_amount(&data));
    Ok(Some(VaultView {
        vault_config_pda,
        cfg,
        vault_authority,
        vault_token,
        vault_token_balance,
    }))
}

/// `--confirm` sends; anything else is a dry run. The DEFAULT is dry-run (asserted in four docs, and pinned by
/// [`tests`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Confirm,
}

impl Mode {
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Mode {
        if args.into_iter().any(|a| a == "--confirm") {
            Mode::Confirm
        } else {
            Mode::DryRun
        }
    }
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was sent (dry run).
    DryRun,
    /// `send` ran once and returned this signature string.
    Sent(String),
}

/// Fetches and decodes `vault_config` at its derived address: `Ok(None)` = never initialised; a failed
/// read and an undecodable account are each refused by their own name.
fn read_vault_config<C: VaultChain>(
    chain: &C,
    bridge: &Pubkey,
    settlement: &Pubkey,
    chain_id: u64,
) -> Result<Option<(Pubkey, VaultConfigAccount)>, Refusal> {
    let (pda, _) = crate::vault_config_pda(bridge, settlement, chain_id);
    let data = chain
        .account_data(&pda)
        .map_err(|error| Refusal::VaultConfigFetchFailed { pda, error })?;
    match data {
        None => Ok(None),
        Some(bytes) => {
            let cfg = crate::decode_vault_config_account(&bytes).map_err(|e: DecodeError| {
                Refusal::VaultConfigUndecodable {
                    pda,
                    error: e.to_string(),
                }
            })?;
            Ok(Some((pda, cfg)))
        }
    }
}

/// Reads and checks `vault_config`, then builds `Fund` — the settlement check runs BEFORE `fund_ix`.
pub fn plan_fund<C: VaultChain>(
    chain: &C,
    bridge: &Pubkey,
    settlement: &Pubkey,
    chain_id: u64,
    payer: &Pubkey,
    amount: u64,
) -> Result<FundPlan, Refusal> {
    let (vault_config_pda, cfg) = read_vault_config(chain, bridge, settlement, chain_id)?.ok_or(
        Refusal::VaultConfigNotFound {
            pda: crate::vault_config_pda(bridge, settlement, chain_id).0,
        },
    )?;
    // The settlement verification, BEFORE any instruction exists: the vault this address resolves
    // to must record the settlement program this command was given.
    crate::check_vault_settlement(&cfg, settlement).map_err(Refusal::VaultSettlementMismatch)?;
    let funder_token_account = crate::recipient_ata(payer, &cfg.mint);
    let ix = crate::fund_ix(
        bridge,
        payer,
        &funder_token_account,
        settlement,
        chain_id,
        &cfg.mint,
        amount,
    );
    Ok(FundPlan {
        vault_config_pda,
        cfg,
        funder_token_account,
        ix,
    })
}

/// Reads `vault_config`; an existing vault is reported, never re-created; otherwise builds `InitVault`.
pub fn plan_init_vault<C: VaultChain>(
    chain: &C,
    bridge: &Pubkey,
    settlement: &Pubkey,
    chain_id: u64,
    mint: Pubkey,
    mint_decimals: u8,
    authority: &Pubkey,
) -> Result<InitVaultPlan, Refusal> {
    let (vault_config_pda, _) = crate::vault_config_pda(bridge, settlement, chain_id);
    if let Some((_, cfg)) = read_vault_config(chain, bridge, settlement, chain_id)? {
        return Ok(InitVaultPlan::AlreadyInitialized {
            vault_config_pda,
            cfg,
        });
    }
    let ix = crate::init_vault_ix(
        bridge,
        authority,
        chain_id,
        mint,
        mint_decimals,
        *settlement,
        authority,
    );
    Ok(InitVaultPlan::Create {
        vault_config_pda,
        ix,
    })
}

/// Sends `ix` through `send` exactly once in [`Mode::Confirm`]; never touches it in [`Mode::DryRun`].
pub fn execute<S: FnMut(&Instruction) -> Result<String, String>>(
    ix: &Instruction,
    mode: Mode,
    send: S,
) -> Result<Outcome, String> {
    match mode {
        Mode::DryRun => Ok(Outcome::DryRun),
        Mode::Confirm => {
            let mut send = send;
            send(ix).map(Outcome::Sent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VaultConfigAccount;

    struct FakeChain {
        data: Option<Vec<u8>>,
        fail: bool,
    }
    impl VaultChain for FakeChain {
        fn account_data(&self, _pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
            if self.fail {
                return Err("connection refused".to_string());
            }
            Ok(self.data.clone())
        }
    }

    /// Encodes with the PROGRAM's own writer (`zk_bridge::state::vault_config::write`) — the bytes the
    /// tool decodes are the bytes the program writes, never a second layout.
    fn encode(cfg: &VaultConfigAccount) -> Vec<u8> {
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id: cfg.chain_id,
            settlement_program: cfg.settlement_program,
            mint: cfg.mint,
            mint_decimals: cfg.mint_decimals,
            authority: cfg.authority,
        })
        .to_vec()
    }

    fn cfg_with_settlement(settlement: Pubkey, mint: Pubkey) -> VaultConfigAccount {
        VaultConfigAccount {
            chain_id: 200_101,
            settlement_program: settlement,
            mint,
            mint_decimals: 6,
            authority: Pubkey::new_unique(),
        }
    }

    // The settlement check is the ONLY guard and it runs before any instruction exists. Mutation target: drop
    // `check_vault_settlement` from `plan_fund`.
    #[test]
    fn plan_fund_refuses_settlement_mismatch_before_building_any_instruction() {
        let bridge = Pubkey::new_unique();
        let settlement_a = Pubkey::new_unique();
        let settlement_b = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let cfg = cfg_with_settlement(settlement_b, mint);
        let chain = FakeChain {
            data: Some(encode(&cfg)),
            fail: false,
        };
        let err = plan_fund(
            &chain,
            &bridge,
            &settlement_a,
            200_101,
            &Pubkey::new_unique(),
            5,
        )
        .expect_err("a vault recording another settlement program must be refused");
        assert!(
            matches!(err, Refusal::VaultSettlementMismatch(_)),
            "refused by name, got {err:?}"
        );
        assert!(err.to_string().starts_with("VaultSettlementMismatch"));
    }

    #[test]
    fn plan_fund_refuses_a_missing_vault_by_name() {
        let chain = FakeChain {
            data: None,
            fail: false,
        };
        let err = plan_fund(
            &chain,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            200_101,
            &Pubkey::new_unique(),
            5,
        )
        .expect_err("no vault");
        assert!(
            matches!(err, Refusal::VaultConfigNotFound { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn plan_fund_reports_a_failed_read_as_a_fetch_failure_not_a_missing_vault() {
        let chain = FakeChain {
            data: None,
            fail: true,
        };
        let err = plan_fund(
            &chain,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            200_101,
            &Pubkey::new_unique(),
            5,
        )
        .expect_err("read failed");
        assert!(
            matches!(err, Refusal::VaultConfigFetchFailed { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn plan_fund_with_a_matching_vault_builds_fund_for_the_payers_ata() {
        let bridge = Pubkey::new_unique();
        let settlement = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let payer = Pubkey::new_unique();
        let cfg = cfg_with_settlement(settlement, mint);
        let chain = FakeChain {
            data: Some(encode(&cfg)),
            fail: false,
        };
        let plan = plan_fund(&chain, &bridge, &settlement, 200_101, &payer, 5).expect("plan");
        assert_eq!(
            plan.funder_token_account,
            crate::recipient_ata(&payer, &mint)
        );
        let expected = crate::fund_ix(
            &bridge,
            &payer,
            &plan.funder_token_account,
            &settlement,
            200_101,
            &mint,
            5,
        );
        assert_eq!(
            plan.ix, expected,
            "the instruction is fund_ix's own, never retyped"
        );
    }

    // show-vault's decisions live here, not inline in the example.
    #[test]
    fn describe_vault_reports_not_initialized_then_the_view_with_a_balance() {
        let bridge = Pubkey::new_unique();
        let settlement = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let cfg = cfg_with_settlement(settlement, mint);
        let none = FakeChain {
            data: None,
            fail: false,
        };
        assert_eq!(
            describe_vault(&none, &bridge, &settlement, 200_101).expect("read"),
            None
        );

        struct TwoAccounts {
            cfg_pda: Pubkey,
            cfg_bytes: Vec<u8>,
            token_pda: Pubkey,
            token_bytes: Vec<u8>,
        }
        impl VaultChain for TwoAccounts {
            fn account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
                if *pubkey == self.cfg_pda {
                    Ok(Some(self.cfg_bytes.clone()))
                } else if *pubkey == self.token_pda {
                    Ok(Some(self.token_bytes.clone()))
                } else {
                    Ok(None)
                }
            }
        }
        let (cfg_pda, _) = crate::vault_config_pda(&bridge, &settlement, 200_101);
        let (token_pda, _) = crate::vault_token_pda(&bridge, &settlement, 200_101, &mint);
        // An SPL token account's amount sits at bytes 64..72 (little-endian u64) — the same offset
        // `zk_bridge::token::read_token_amount` reads.
        let mut token_bytes = vec![0u8; 165];
        token_bytes[64..72].copy_from_slice(&1_234_567u64.to_le_bytes());
        let chain = TwoAccounts {
            cfg_pda,
            cfg_bytes: encode(&cfg),
            token_pda,
            token_bytes,
        };
        let view = describe_vault(&chain, &bridge, &settlement, 200_101)
            .expect("read")
            .expect("initialized");
        assert_eq!(view.cfg, cfg);
        assert_eq!(view.vault_token, token_pda);
        assert_eq!(view.vault_token_balance, Some(1_234_567));
    }

    #[test]
    fn plan_init_vault_never_recreates_an_existing_vault() {
        let bridge = Pubkey::new_unique();
        let settlement = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let cfg = cfg_with_settlement(settlement, mint);
        let chain = FakeChain {
            data: Some(encode(&cfg)),
            fail: false,
        };
        let plan = plan_init_vault(
            &chain,
            &bridge,
            &settlement,
            200_101,
            mint,
            6,
            &Pubkey::new_unique(),
        )
        .expect("plan");
        assert!(
            matches!(plan, InitVaultPlan::AlreadyInitialized { .. }),
            "{plan:?}"
        );
        let chain = FakeChain {
            data: None,
            fail: false,
        };
        let plan = plan_init_vault(
            &chain,
            &bridge,
            &settlement,
            200_101,
            mint,
            6,
            &Pubkey::new_unique(),
        )
        .expect("plan");
        assert!(matches!(plan, InitVaultPlan::Create { .. }), "{plan:?}");
    }

    // "--dry-run is the DEFAULT" pinned by a test. Mutation target: flip the default in `Mode::from_args`.
    #[test]
    fn dry_run_is_the_default_and_never_calls_send() {
        let mode = Mode::from_args([
            "vault".to_string(),
            "fund".to_string(),
            "--amount".to_string(),
            "5".to_string(),
        ]);
        assert_eq!(mode, Mode::DryRun);
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        };
        let out = execute(&ix, mode, |_| {
            panic!("send must never be called in a dry run")
        })
        .expect("dry run");
        assert_eq!(out, Outcome::DryRun);
    }

    #[test]
    fn confirm_calls_send_exactly_once() {
        let mode = Mode::from_args([
            "vault".to_string(),
            "fund".to_string(),
            "--confirm".to_string(),
        ]);
        assert_eq!(mode, Mode::Confirm);
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        };
        let mut calls = 0;
        let out = execute(&ix, mode, |_| {
            calls += 1;
            Ok("sig".to_string())
        })
        .expect("sent");
        assert_eq!(calls, 1);
        assert_eq!(out, Outcome::Sent("sig".to_string()));
    }
}
