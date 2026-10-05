//! The bridge commands: `vault init`, `vault fund`, `vault show`, `release-exit` and `deposit`. The planning (what to refuse,
//! in which order, what to build) is `zk_bridge_client::vault_tool` and the instruction builders in `zk_bridge_client`;
//! this module only reads the accounts those functions need, maps their refusals to named errors, and sends through
//! [`crate::commands::execute_many`], so every send is one V1 transaction through `rome-zk-solana-sender`.
//!
//! Every chain read happens before any key file is opened, and a failed read stops the command by name in a dry run
//! as well as a confirmed one: there is nothing to build without it, and a dry run that went on without it would
//! print a transaction for an account that was never looked at.

use crate::chain::Chain;
use crate::commands::execute_many;
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;
use std::path::PathBuf;
use zk_bridge_client::vault_tool::{
    describe_vault, plan_fund, plan_init_vault, InitVaultPlan, Refusal, VaultChain,
};

/// The accounts read up front, handed to the synchronous planners in `vault_tool`. A key that was not read up front
/// is an error, never "does not exist".
struct Prefetched(HashMap<Pubkey, Option<Vec<u8>>>);

impl VaultChain for Prefetched {
    fn account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        match self.0.get(pubkey) {
            Some(d) => Ok(d.clone()),
            None => Err(format!("{pubkey} was not read before planning")),
        }
    }
}

/// Reads one account and refuses by `name` when the request itself fails, in either mode. `Ok(None)` is an account
/// that does not exist.
async fn fetch<C: Chain>(
    chain: &C,
    key: &Pubkey,
    what: &str,
    name: &'static str,
) -> Result<Option<Vec<u8>>, OpsError> {
    chain.account(key).await.map_err(|e| {
        OpsError::chain(
            name,
            format!(
                "the RPC request failed, so {what} {key} could not be read (this is not a missing account); the client said: {e}"
            ),
        )
    })
}

/// The refusals `vault_tool` raises, as named errors. Their text already starts with their name.
fn refusal(r: Refusal) -> OpsError {
    let (name, text): (&'static str, String) = match &r {
        Refusal::VaultConfigFetchFailed { .. } => ("VaultConfigFetchFailed", r.to_string()),
        Refusal::VaultConfigNotFound { .. } => ("VaultConfigNotFound", r.to_string()),
        Refusal::VaultConfigUndecodable { .. } => ("VaultConfigUndecodable", r.to_string()),
        Refusal::VaultSettlementMismatch(_) => ("VaultSettlementMismatch", r.to_string()),
    };
    let detail = text
        .strip_prefix(&format!("{name}: "))
        .unwrap_or(&text)
        .to_string();
    OpsError::chain(name, detail)
}

fn print_account(report: &mut Report, pubkey: &Pubkey, writable: bool, signer: bool) {
    report.line(format!(
        "      {pubkey}{}{}",
        if writable { "  (writable)" } else { "" },
        if signer { "  (signer)" } else { "" },
    ));
}

fn print_accounts(report: &mut Report, ix: &solana_program::instruction::Instruction) {
    for m in &ix.accounts {
        print_account(report, &m.pubkey, m.is_writable, m.is_signer);
    }
}

fn print_vault_config(
    report: &mut Report,
    cfg: &zk_bridge_client::VaultConfigAccount,
    pda: &Pubkey,
) {
    report.line(format!("  vault_config   {pda}"));
    report.line(format!("    chain_id             {}", cfg.chain_id));
    report.line(format!(
        "    settlement_program   {}",
        cfg.settlement_program
    ));
    report.line(format!("    mint                 {}", cfg.mint));
    report.line(format!("    mint_decimals        {}", cfg.mint_decimals));
    report.line(format!("    authority            {}", cfg.authority));
}

pub struct VaultInitRequest {
    pub authority_keypair: PathBuf,
    pub mint: Pubkey,
    pub mint_decimals: u8,
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
}

/// `vault init`: create the vault for a chain. An existing vault is reported and nothing is sent.
pub async fn vault_init<C: Chain>(
    chain: &C,
    req: VaultInitRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (pda, _) = zk_bridge_client::vault_config_pda(&req.bridge, &req.settlement, req.chain_id);
    let data = fetch(chain, &pda, "vault_config", "VaultConfigCheckFailed").await?;
    let prefetched = Prefetched(HashMap::from([(pda, data)]));
    let authority = keys::load(&req.authority_keypair, "--authority-keypair")?;
    let authority_key = keys::pubkey(&authority);
    let plan = plan_init_vault(
        &prefetched,
        &req.bridge,
        &req.settlement,
        req.chain_id,
        req.mint,
        req.mint_decimals,
        &authority_key,
    )
    .map_err(refusal)?;
    match plan {
        InitVaultPlan::AlreadyInitialized {
            vault_config_pda,
            cfg,
        } => {
            report.line("vault_config already initialized, nothing sent:");
            print_vault_config(&mut report, &cfg, &vault_config_pda);
        }
        InitVaultPlan::Create {
            vault_config_pda,
            ix,
        } => {
            report.line(format!(
                "-- vault init: chain {}, settlement {}, bridge {}, mint {}",
                req.chain_id, req.settlement, req.bridge, req.mint
            ));
            report.line(format!(
                "  vault_config (not yet created): {vault_config_pda}"
            ));
            report.line("  instruction: InitVault");
            print_accounts(&mut report, &ix);
            let signers = Signers::new(authority, vec![]);
            if let Some(sig) = execute_many(
                chain,
                mode,
                "InitVault",
                std::slice::from_ref(&ix),
                &signers,
                &mut report,
            )
            .await?
            {
                report.line(format!("-- sent: {sig}"));
            }
        }
    }
    Ok(report)
}

pub struct VaultFundRequest {
    pub payer_keypair: PathBuf,
    pub amount: u64,
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
}

/// `vault fund`: move `amount` raw units from the payer's token account into the vault. The vault must exist and
/// must record the settlement program this command was given.
pub async fn vault_fund<C: Chain>(
    chain: &C,
    req: VaultFundRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (pda, _) = zk_bridge_client::vault_config_pda(&req.bridge, &req.settlement, req.chain_id);
    let data = fetch(chain, &pda, "vault_config", "VaultConfigFetchFailed").await?;
    let prefetched = Prefetched(HashMap::from([(pda, data)]));
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let plan = plan_fund(
        &prefetched,
        &req.bridge,
        &req.settlement,
        req.chain_id,
        &keys::pubkey(&payer),
        req.amount,
    )
    .map_err(refusal)?;
    report.line(format!(
        "-- vault fund: chain {}, settlement {}, bridge {}, amount {}",
        req.chain_id, req.settlement, req.bridge, req.amount
    ));
    print_vault_config(&mut report, &plan.cfg, &plan.vault_config_pda);
    report.line(format!(
        "  funder_token_account  {}",
        plan.funder_token_account
    ));
    report.line("  instruction: Fund");
    print_accounts(&mut report, &plan.ix);
    let signers = Signers::new(payer, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "Fund",
        std::slice::from_ref(&plan.ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

/// `vault show`: print the vault's configuration and its token balance. Reads only.
pub async fn vault_show<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    bridge: &Pubkey,
    chain_id: u64,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (pda, _) = zk_bridge_client::vault_config_pda(bridge, settlement, chain_id);
    let data = fetch(chain, &pda, "vault_config", "VaultConfigFetchFailed").await?;
    let mut reads = HashMap::from([(pda, data.clone())]);
    // The token account's address depends on the mint the vault records, so it is read second. A vault that is
    // missing or undecodable is left for `describe_vault` to refuse by name.
    if let Some(Ok(cfg)) = data
        .as_deref()
        .map(zk_bridge_client::decode_vault_config_account)
    {
        let (token, _) = zk_bridge_client::vault_token_pda(bridge, settlement, chain_id, &cfg.mint);
        let token_data = fetch(
            chain,
            &token,
            "the vault token account",
            "VaultTokenFetchFailed",
        )
        .await?;
        reads.insert(token, token_data);
    }
    match describe_vault(&Prefetched(reads), bridge, settlement, chain_id).map_err(refusal)? {
        Some(view) => {
            print_vault_config(&mut report, &view.cfg, &view.vault_config_pda);
            report.line(format!("  vault_authority      {}", view.vault_authority));
            match view.vault_token_balance {
                Some(balance) => report.line(format!(
                    "  vault_token          {}  balance {balance} (raw units, {} decimals)",
                    view.vault_token, view.cfg.mint_decimals
                )),
                None => report.line(format!(
                    "  vault_token          {}  not found (InitVault should have created it)",
                    view.vault_token
                )),
            }
        }
        None => report.line(format!(
            "vault_config {pda} (chain {chain_id}, settlement {settlement}, bridge {bridge}): not initialized"
        )),
    }
    Ok(report)
}

pub struct ReleaseExitRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    pub message_hash: [u8; 32],
    pub payer_keypair: PathBuf,
}

/// `release-exit`: read a PROVED settlement `exit_record`, derive the recipient's token account for the vault's mint,
/// and send `ReleaseExit` after an idempotent create of that token account (the program derives and checks it but
/// never creates it). Both instructions go in one V1 transaction.
pub async fn release_exit<C: Chain>(
    chain: &C,
    req: ReleaseExitRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (record_pda, _) =
        zk_settlement_client::exit_record_pda(&req.settlement, req.chain_id, req.message_hash);
    let record_data = fetch(chain, &record_pda, "exit_record", "ExitRecordLookupFailed")
        .await?
        .ok_or_else(|| {
            OpsError::chain(
                "ExitRecordNotFound",
                format!(
                    "exit_record {record_pda} not found (never proved, or already released and closed)"
                ),
            )
        })?;
    let record = zk_settlement_client::decode_exit_record_account(&record_data).map_err(|e| {
        OpsError::chain(
            "ExitRecordUndecodable",
            format!("exit_record {record_pda} does not decode: {e}"),
        )
    })?;
    if record.status != rome_zk_layouts::exit::exit_record::STATUS_PROVED {
        return Err(OpsError::chain(
            "ExitRecordNotProved",
            format!(
                "exit_record {record_pda} is not PROVED (status {}); ReleaseExit only accepts a freshly proved record",
                record.status
            ),
        ));
    }

    let (vault_pda, _) =
        zk_bridge_client::vault_config_pda(&req.bridge, &req.settlement, req.chain_id);
    let vault_data = fetch(chain, &vault_pda, "vault_config", "VaultConfigFetchFailed")
        .await?
        .ok_or_else(|| {
            OpsError::chain(
                "VaultConfigNotFound",
                format!("vault_config {vault_pda} not found; run `vault init` first"),
            )
        })?;
    let vault = zk_bridge_client::decode_vault_config_account(&vault_data).map_err(|e| {
        OpsError::chain(
            "VaultConfigUndecodable",
            format!("vault_config {vault_pda} does not decode: {e}"),
        )
    })?;

    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let recipient = Pubkey::new_from_array(record.sol_recipient);
    let (exit_config, _) = zk_settlement_client::exit_config_pda(&req.settlement, req.chain_id);
    let ata_ix = zk_bridge_client::create_recipient_ata_idempotent_ix(
        &keys::pubkey(&payer),
        &recipient,
        &vault.mint,
    );
    let release_ix = zk_bridge_client::release_exit_ix(
        &req.bridge,
        &req.settlement,
        req.chain_id,
        req.message_hash,
        &vault.mint,
        &exit_config,
        &record_pda,
        &record.payer,
        &recipient,
    );

    report.line(format!(
        "-- release-exit: chain {}, message_hash 0x{}",
        req.chain_id,
        hex::encode(req.message_hash)
    ));
    report.line(format!("  recipient      {recipient}"));
    report.line(format!(
        "  recipient_ata  {}",
        zk_bridge_client::recipient_ata(&recipient, &vault.mint)
    ));
    report.line(format!("  amount (wei)   {}", record.amount));
    report.line(format!("  mint           {}", vault.mint));
    report.line("  instruction 1: CreateIdempotent (Associated Token Program)");
    print_accounts(&mut report, &ata_ix);
    report.line("  instruction 2: ReleaseExit (zk-bridge)");
    print_accounts(&mut report, &release_ix);

    let signers = Signers::new(payer, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "ReleaseExit",
        &[ata_ix, release_ix],
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

pub struct DepositRequest {
    pub settlement: Pubkey,
    pub bridge: Pubkey,
    pub chain_id: u64,
    /// Raw units of the vault's mint (lamports when the vault holds wrapped SOL).
    pub amount: u64,
    /// The 20-byte address that is credited on the chain.
    pub l2_recipient: [u8; 20],
    /// Wrap `amount` lamports of the depositor's SOL first, in the same transaction. Only for a vault whose mint is
    /// wrapped SOL.
    pub wrap_sol: bool,
    /// The depositor's keypair file; it signs and pays the record's rent and the fee.
    pub keypair: PathBuf,
}

/// `deposit`: lock `amount` of the vault's mint and queue a credit of the same value, in gwei, to `l2_recipient`.
/// Reads the vault (for the mint) and the queue (for the next index, the minimum and the fee recipient) before any
/// key file is opened, then sends `Deposit`, preceded by the wrap instructions with `--wrap-sol`, as one V1
/// transaction.
pub async fn deposit<C: Chain>(
    chain: &C,
    req: DepositRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (vault_pda, _) =
        zk_bridge_client::vault_config_pda(&req.bridge, &req.settlement, req.chain_id);
    let vault_data = fetch(chain, &vault_pda, "vault_config", "VaultConfigFetchFailed")
        .await?
        .ok_or_else(|| {
            OpsError::chain(
                "VaultConfigNotFound",
                format!("vault_config {vault_pda} not found; the chain has no bridge vault under this bridge"),
            )
        })?;
    let vault = zk_bridge_client::decode_vault_config_account(&vault_data).map_err(|e| {
        OpsError::chain(
            "VaultConfigUndecodable",
            format!("vault_config {vault_pda} does not decode: {e}"),
        )
    })?;
    check_vault_settlement(&vault, &req.settlement)?;

    let (queue_pda, _) =
        zk_bridge_client::deposit_queue_pda(&req.bridge, &req.settlement, req.chain_id);
    let queue_data = fetch(chain, &queue_pda, "deposit_queue", "DepositQueueFetchFailed")
        .await?
        .ok_or_else(|| {
            OpsError::chain(
                "DepositQueueNotFound",
                format!("deposit_queue {queue_pda} not found; the chain's deposit queue has not been set up"),
            )
        })?;
    let queue = rome_zk_layouts::deposit_queue::deposit_queue::read(&queue_data).map_err(|e| {
        OpsError::chain(
            "DepositQueueUndecodable",
            format!("deposit_queue {queue_pda} does not decode: {e:?}"),
        )
    })?;

    if req.amount < queue.params.min_amount {
        return Err(OpsError::chain(
            "DepositBelowMinimum",
            format!(
                "amount {} is below the queue's minimum {} (raw units)",
                req.amount, queue.params.min_amount
            ),
        ));
    }
    if req.l2_recipient == [0u8; 20] {
        return Err(OpsError::chain(
            "DepositRecipientInvalid",
            "the recipient is the zero address".to_string(),
        ));
    }
    if req.wrap_sol && vault.mint != zk_bridge_client::NATIVE_MINT {
        return Err(OpsError::chain(
            "WrapSolNeedsNativeMint",
            format!(
                "--wrap-sol was given but the vault's mint is {}, not wrapped SOL",
                vault.mint
            ),
        ));
    }
    let gwei = rome_zk_layouts::deposit_queue::amount_gwei(req.amount, vault.mint_decimals)
        .map_err(|e| OpsError::chain("DepositAmountInvalid", format!("{e:?}")))?;

    let depositor = keys::load(&req.keypair, "--keypair")?;
    let depositor_key = keys::pubkey(&depositor);
    let index = queue.count;
    let fee_recipient = Pubkey::new_from_array(queue.params.fee_recipient);
    let mut ixs = Vec::new();
    let depositor_token = if req.wrap_sol {
        let (ata, wrap) = zk_bridge_client::wrap_sol_ixs(&depositor_key, req.amount);
        ixs.extend(wrap);
        ata
    } else {
        zk_bridge_client::recipient_ata(&depositor_key, &vault.mint)
    };
    let deposit_ix = zk_bridge_client::deposit_ix(
        &req.bridge,
        &depositor_key,
        &depositor_token,
        &req.settlement,
        req.chain_id,
        &vault.mint,
        index,
        &fee_recipient,
        req.amount,
        req.l2_recipient,
    );
    ixs.push(deposit_ix.clone());

    report.line(format!(
        "-- deposit: chain {}, settlement {}, bridge {}",
        req.chain_id, req.settlement, req.bridge
    ));
    report.line(format!("  depositor        {depositor_key}"));
    report.line(format!(
        "  recipient        0x{}",
        hex::encode(req.l2_recipient)
    ));
    report.line(format!(
        "  amount           {} raw units of {} ({gwei} gwei credited)",
        req.amount, vault.mint
    ));
    report.line(format!(
        "  record index     {index} (the queue's current count)"
    ));
    report.line(format!(
        "  fee              {} lamports to {fee_recipient}",
        queue.params.fee_lamports
    ));
    if req.wrap_sol {
        report.line(
            "  instructions 1-3: wrap SOL (create the token account if missing, transfer, sync)",
        );
    }
    report.line("  instruction: Deposit (zk-bridge)");
    print_accounts(&mut report, &deposit_ix);

    let signers = Signers::new(depositor, vec![]);
    if let Some(sig) = execute_many(chain, mode, "Deposit", &ixs, &signers, &mut report).await? {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

fn check_vault_settlement(
    vault: &zk_bridge_client::VaultConfigAccount,
    settlement: &Pubkey,
) -> Result<(), OpsError> {
    zk_bridge_client::check_vault_settlement(vault, settlement).map_err(|e| {
        OpsError::chain(
            "VaultSettlementMismatch",
            e.to_string()
                .trim_start_matches("VaultSettlementMismatch: ")
                .to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    use zk_bridge::state::vault_config::{write, VaultConfigFields};

    const CHAIN: u64 = 200_101;

    fn bridge() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn vault_bytes(settlement: Pubkey, mint: Pubkey) -> Vec<u8> {
        write(&VaultConfigFields {
            chain_id: CHAIN,
            settlement_program: settlement,
            mint,
            mint_decimals: 6,
            authority: Pubkey::new_from_array([5u8; 32]),
        })
        .to_vec()
    }

    fn token_account(amount: u64) -> Vec<u8> {
        let mut d = vec![0u8; 165];
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d
    }

    fn vault_pda() -> Pubkey {
        zk_bridge_client::vault_config_pda(&bridge(), &program(), CHAIN).0
    }

    fn record_pda(hash: [u8; 32]) -> Pubkey {
        zk_settlement_client::exit_record_pda(&program(), CHAIN, hash).0
    }

    fn record_bytes(status: u8, recipient: Pubkey, payer: Pubkey, hash: [u8; 32]) -> Vec<u8> {
        rome_zk_layouts::exit::exit_record::write(
            &rome_zk_layouts::exit::exit_record::ExitRecordFields {
                chain_id: CHAIN,
                batch: 3,
                message_hash: hash,
                sol_recipient: recipient.to_bytes(),
                amount: 1_500_000,
                window_index: 0,
                proved_slot: 10,
                status,
                payer: payer.to_bytes(),
                asset: [0u8; 20],
            },
        )
        .to_vec()
    }

    fn init_req(path: PathBuf, mint: Pubkey) -> VaultInitRequest {
        VaultInitRequest {
            authority_keypair: path,
            mint,
            mint_decimals: 6,
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
        }
    }

    fn fund_req(path: PathBuf, amount: u64) -> VaultFundRequest {
        VaultFundRequest {
            payer_keypair: path,
            amount,
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
        }
    }

    fn release_req(path: PathBuf, hash: [u8; 32]) -> ReleaseExitRequest {
        ReleaseExitRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            message_hash: hash,
            payer_keypair: path,
        }
    }

    // ---- vault init ----

    #[tokio::test]
    async fn init_dry_run_prints_the_transaction_and_sends_nothing() {
        let (_, path) = key_file("vinit-dry");
        let chain = FakeChain::default();
        let report = vault_init(
            &chain,
            init_req(path.clone(), Pubkey::new_unique()),
            Mode::Dry,
        )
        .await
        .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("instruction: InitVault"), "{text}");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
        assert!(text.contains("tx (base64)"), "{text}");
    }

    #[tokio::test]
    async fn init_confirm_sends_one_transaction_with_one_instruction() {
        let (_, path) = key_file("vinit-send");
        let chain = FakeChain::default();
        let report = vault_init(
            &chain,
            init_req(path.clone(), Pubkey::new_unique()),
            Mode::Confirm,
        )
        .await
        .unwrap();
        remove(&path);
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(chain.sent_count(), 1);
        assert!(report.sent());
        assert!(report
            .lines
            .join("\n")
            .contains("-- sent: FakeSignature1111"));
    }

    #[tokio::test]
    async fn init_on_an_existing_vault_reports_it_and_sends_nothing() {
        let (_, path) = key_file("vinit-exists");
        let mint = Pubkey::new_unique();
        let chain = FakeChain::default().with(vault_pda(), vault_bytes(program(), mint));
        let report = vault_init(&chain, init_req(path.clone(), mint), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        assert!(!report.sent());
        assert!(report.lines.join("\n").contains("already initialized"));
    }

    #[tokio::test]
    async fn init_refuses_by_name_when_the_read_fails_in_both_modes_before_any_key_is_read() {
        for mode in [Mode::Dry, Mode::Confirm] {
            let chain = FakeChain::default().failing(vault_pda(), "connection refused");
            let err = vault_init(
                &chain,
                init_req(
                    PathBuf::from("/nonexistent/authority.json"),
                    Pubkey::new_unique(),
                ),
                mode,
            )
            .await
            .unwrap_err();
            assert_eq!(err.name, "VaultConfigCheckFailed", "{mode:?}");
            assert!(err.detail.contains("not a missing account"), "{err}");
            assert_eq!(chain.sent_count(), 0);
        }
    }

    #[tokio::test]
    async fn init_refuses_an_unreadable_key_by_name() {
        let chain = FakeChain::default();
        let err = vault_init(
            &chain,
            init_req(
                PathBuf::from("/nonexistent/authority.json"),
                Pubkey::new_unique(),
            ),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "KeypairUnreadable");
        assert_eq!(chain.sent_count(), 0);
    }

    // ---- vault fund ----

    #[tokio::test]
    async fn fund_dry_run_sends_nothing_and_confirm_sends_one() {
        let (_, path) = key_file("vfund");
        let mint = Pubkey::new_unique();
        let chain = FakeChain::default().with(vault_pda(), vault_bytes(program(), mint));
        let report = vault_fund(&chain, fund_req(path.clone(), 5), Mode::Dry)
            .await
            .unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(report.lines.join("\n").contains("instruction: Fund"));
        vault_fund(&chain, fund_req(path.clone(), 5), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(chain.sent_count(), 1);
    }

    #[tokio::test]
    async fn fund_refuses_a_settlement_mismatch_before_building_anything() {
        let (_, path) = key_file("vfund-mismatch");
        let other = Pubkey::new_unique();
        let chain =
            FakeChain::default().with(vault_pda(), vault_bytes(other, Pubkey::new_unique()));
        let err = vault_fund(&chain, fund_req(path.clone(), 5), Mode::Confirm)
            .await
            .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "VaultSettlementMismatch");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn fund_refuses_a_missing_vault_by_name() {
        let (_, path) = key_file("vfund-none");
        let chain = FakeChain::default();
        let err = vault_fund(&chain, fund_req(path.clone(), 5), Mode::Confirm)
            .await
            .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "VaultConfigNotFound");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn fund_refuses_by_name_when_the_read_fails_in_both_modes() {
        for mode in [Mode::Dry, Mode::Confirm] {
            let chain = FakeChain::default().failing(vault_pda(), "connection refused");
            let err = vault_fund(
                &chain,
                fund_req(PathBuf::from("/nonexistent/payer.json"), 5),
                mode,
            )
            .await
            .unwrap_err();
            assert_eq!(err.name, "VaultConfigFetchFailed", "{mode:?}");
            assert_eq!(chain.sent_count(), 0);
        }
    }

    // ---- vault show ----

    #[tokio::test]
    async fn show_prints_the_vault_and_its_balance() {
        let mint = Pubkey::new_unique();
        let (token, _) = zk_bridge_client::vault_token_pda(&bridge(), &program(), CHAIN, &mint);
        let chain = FakeChain::default()
            .with(vault_pda(), vault_bytes(program(), mint))
            .with(token, token_account(42));
        let report = vault_show(&chain, &program(), &bridge(), CHAIN)
            .await
            .unwrap();
        let text = report.lines.join("\n");
        assert!(text.contains(&mint.to_string()), "{text}");
        assert!(text.contains("balance 42"), "{text}");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn show_says_so_when_the_vault_does_not_exist() {
        let chain = FakeChain::default();
        let report = vault_show(&chain, &program(), &bridge(), CHAIN)
            .await
            .unwrap();
        assert!(report.lines.join("\n").contains("not initialized"));
    }

    #[tokio::test]
    async fn show_names_a_failed_token_account_read() {
        let mint = Pubkey::new_unique();
        let (token, _) = zk_bridge_client::vault_token_pda(&bridge(), &program(), CHAIN, &mint);
        let chain = FakeChain::default()
            .with(vault_pda(), vault_bytes(program(), mint))
            .failing(token, "timeout");
        let err = vault_show(&chain, &program(), &bridge(), CHAIN)
            .await
            .unwrap_err();
        assert_eq!(err.name, "VaultTokenFetchFailed");
    }

    // ---- release-exit ----

    fn release_world(status: u8) -> (FakeChain, Pubkey, Pubkey, [u8; 32]) {
        let hash = [0x77u8; 32];
        let recipient = Pubkey::new_from_array([3u8; 32]);
        let refund = Pubkey::new_from_array([4u8; 32]);
        let mint = Pubkey::new_unique();
        let chain = FakeChain::default()
            .with(
                record_pda(hash),
                record_bytes(status, recipient, refund, hash),
            )
            .with(vault_pda(), vault_bytes(program(), mint));
        (chain, recipient, mint, hash)
    }

    #[tokio::test]
    async fn release_dry_run_prints_both_instructions_and_sends_nothing() {
        let (chain, recipient, mint, hash) = release_world(1);
        let (_, path) = key_file("rel-dry");
        let report = release_exit(&chain, release_req(path.clone(), hash), Mode::Dry)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains(&recipient.to_string()), "{text}");
        assert!(text.contains(&mint.to_string()), "{text}");
        assert!(text.contains("instruction 1: CreateIdempotent"), "{text}");
        assert!(text.contains("instruction 2: ReleaseExit"), "{text}");
        assert!(text.contains("tx (base64)"), "{text}");
    }

    #[tokio::test]
    async fn release_confirm_sends_the_ata_create_and_the_release_in_one_transaction() {
        let (chain, recipient, mint, hash) = release_world(1);
        let (_, path) = key_file("rel-send");
        let report = release_exit(&chain, release_req(path.clone(), hash), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(chain.tx_sizes.lock().unwrap()[0], 2);
        let sent = chain.sent.lock().unwrap().clone();
        assert_eq!(
            sent[0].program_id,
            zk_bridge::token::ASSOCIATED_TOKEN_PROGRAM_ID
        );
        assert!(sent[0]
            .accounts
            .iter()
            .any(|a| a.pubkey == zk_bridge_client::recipient_ata(&recipient, &mint)));
        assert_eq!(sent[1].program_id, bridge());
        assert!(report.sent());
    }

    #[tokio::test]
    async fn release_refuses_a_record_that_is_not_proved() {
        let (chain, _, _, hash) = release_world(2);
        let (_, path) = key_file("rel-released");
        let err = release_exit(&chain, release_req(path.clone(), hash), Mode::Confirm)
            .await
            .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "ExitRecordNotProved");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn release_refuses_a_missing_record_and_a_failed_read_by_different_names() {
        let hash = [0x11u8; 32];
        let (_, path) = key_file("rel-missing");
        let chain = FakeChain::default();
        let err = release_exit(&chain, release_req(path.clone(), hash), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ExitRecordNotFound");
        for mode in [Mode::Dry, Mode::Confirm] {
            let chain = FakeChain::default().failing(record_pda(hash), "connection refused");
            let err = release_exit(&chain, release_req(path.clone(), hash), mode)
                .await
                .unwrap_err();
            assert_eq!(err.name, "ExitRecordLookupFailed", "{mode:?}");
            assert!(err.detail.contains("exit_record"), "{err}");
            assert_eq!(chain.sent_count(), 0);
        }
        remove(&path);
    }

    #[tokio::test]
    async fn release_refuses_a_missing_vault_by_name() {
        let hash = [0x22u8; 32];
        let (_, path) = key_file("rel-novault");
        let chain = FakeChain::default().with(
            record_pda(hash),
            record_bytes(1, Pubkey::new_unique(), Pubkey::new_unique(), hash),
        );
        let err = release_exit(&chain, release_req(path.clone(), hash), Mode::Confirm)
            .await
            .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "VaultConfigNotFound");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn release_reads_the_chain_before_it_opens_the_key() {
        let hash = [0x33u8; 32];
        let chain = FakeChain::default();
        let err = release_exit(
            &chain,
            release_req(PathBuf::from("/nonexistent/payer.json"), hash),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "ExitRecordNotFound");
    }

    // ---- deposit ----

    const RECIPIENT: [u8; 20] = [0xab; 20];

    fn queue_bytes(count: u64, min_amount: u64, fee_recipient: Pubkey) -> Vec<u8> {
        use rome_zk_layouts::deposit_queue::deposit_queue as q;
        let mut d = vec![0u8; q::LEN];
        q::write(
            &mut d,
            &q::DepositQueueFields {
                count,
                head_hash: [1u8; 32],
                params: q::DepositParams {
                    inclusion_deadline_secs: 600,
                    max_per_batch: 64,
                    max_per_block: 16,
                    min_amount,
                    fee_lamports: 2_000_000,
                    fee_recipient: fee_recipient.to_bytes(),
                },
                pending: q::DepositParams::default(),
                activation_slot: 0,
            },
        );
        d
    }

    fn queue_pda() -> Pubkey {
        zk_bridge_client::deposit_queue_pda(&bridge(), &program(), CHAIN).0
    }

    fn deposit_req(path: PathBuf, amount: u64, wrap_sol: bool) -> DepositRequest {
        DepositRequest {
            settlement: program(),
            bridge: bridge(),
            chain_id: CHAIN,
            amount,
            l2_recipient: RECIPIENT,
            wrap_sol,
            keypair: path,
        }
    }

    fn deposit_chain(mint: Pubkey, count: u64, fee_recipient: Pubkey) -> FakeChain {
        FakeChain::default()
            .with(vault_pda(), vault_bytes(program(), mint))
            .with(queue_pda(), queue_bytes(count, 1_000, fee_recipient))
    }

    #[tokio::test]
    async fn deposit_dry_run_prints_the_transaction_and_sends_nothing() {
        let (_, path) = key_file("dep-dry");
        let chain = deposit_chain(Pubkey::new_unique(), 4, Pubkey::new_unique());
        let report = deposit(&chain, deposit_req(path.clone(), 5_000, false), Mode::Dry)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("instruction: Deposit"), "{text}");
        assert!(text.contains("record index     4"), "{text}");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
    }

    #[tokio::test]
    async fn deposit_confirm_sends_one_transaction_with_the_deposit_at_the_queues_count() {
        let (k, path) = key_file("dep-send");
        let fee = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let chain = deposit_chain(mint, 7, fee);
        let report = deposit(
            &chain,
            deposit_req(path.clone(), 5_000, false),
            Mode::Confirm,
        )
        .await
        .unwrap();
        remove(&path);
        assert!(report.sent());
        assert_eq!(chain.tx_count(), 1);
        let sent = chain.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let ix = &sent[0];
        assert_eq!(ix.program_id, bridge());
        let depositor = key_pubkey(&k);
        assert_eq!(ix.accounts[0].pubkey, depositor);
        assert_eq!(
            ix.accounts[1].pubkey,
            zk_bridge_client::recipient_ata(&depositor, &mint)
        );
        assert_eq!(
            ix.accounts[5].pubkey,
            zk_bridge_client::deposit_record_pda(&bridge(), &program(), CHAIN, 7).0
        );
        assert_eq!(ix.accounts[7].pubkey, fee);
        // After the registry came in, the program reads thirteen accounts: root, registry and bridge config last.
        assert_eq!(ix.accounts.len(), 13);
        assert_eq!(
            ix.accounts[10].pubkey,
            zk_settlement_client::root_pda(&program(), CHAIN).0
        );
        assert_eq!(
            ix.accounts[11].pubkey,
            rome_zk_layouts::registry::pda(&program(), CHAIN).0
        );
        assert_eq!(
            ix.accounts[12].pubkey,
            zk_bridge_client::bridge_config_pda(&bridge()).0
        );
    }

    #[tokio::test]
    async fn deposit_with_wrap_sol_puts_the_wrap_ahead_of_the_deposit_in_one_transaction() {
        let (_, path) = key_file("dep-wrap");
        let chain = deposit_chain(zk_bridge_client::NATIVE_MINT, 0, Pubkey::new_unique());
        deposit(
            &chain,
            deposit_req(path.clone(), 5_000, true),
            Mode::Confirm,
        )
        .await
        .unwrap();
        remove(&path);
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(chain.sent_count(), 4);
        assert_eq!(chain.sent.lock().unwrap()[3].program_id, bridge());
    }

    #[tokio::test]
    async fn deposit_refuses_wrap_sol_for_a_vault_that_is_not_wrapped_sol() {
        let (_, path) = key_file("dep-wrap-no");
        let chain = deposit_chain(Pubkey::new_unique(), 0, Pubkey::new_unique());
        let err = deposit(
            &chain,
            deposit_req(path.clone(), 5_000, true),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "WrapSolNeedsNativeMint");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn deposit_refuses_below_the_minimum_and_a_zero_recipient_by_name() {
        let (_, path) = key_file("dep-refuse");
        let chain = deposit_chain(Pubkey::new_unique(), 0, Pubkey::new_unique());
        let err = deposit(&chain, deposit_req(path.clone(), 999, false), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "DepositBelowMinimum");
        let mut req = deposit_req(path.clone(), 5_000, false);
        req.l2_recipient = [0u8; 20];
        let err = deposit(&chain, req, Mode::Confirm).await.unwrap_err();
        remove(&path);
        assert_eq!(err.name, "DepositRecipientInvalid");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn deposit_refuses_a_missing_vault_a_missing_queue_and_a_foreign_settlement_by_name() {
        let (_, path) = key_file("dep-missing");
        let chain = FakeChain::default();
        let err = deposit(
            &chain,
            deposit_req(path.clone(), 5_000, false),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "VaultConfigNotFound");

        let chain =
            FakeChain::default().with(vault_pda(), vault_bytes(program(), Pubkey::new_unique()));
        let err = deposit(
            &chain,
            deposit_req(path.clone(), 5_000, false),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "DepositQueueNotFound");

        let chain = FakeChain::default().with(
            vault_pda(),
            vault_bytes(Pubkey::new_unique(), Pubkey::new_unique()),
        );
        let err = deposit(
            &chain,
            deposit_req(path.clone(), 5_000, false),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        remove(&path);
        assert_eq!(err.name, "VaultSettlementMismatch");
    }

    #[tokio::test]
    async fn deposit_reads_the_chain_before_it_opens_the_key_and_names_a_failed_read() {
        for mode in [Mode::Dry, Mode::Confirm] {
            let chain = FakeChain::default().failing(vault_pda(), "connection refused");
            let err = deposit(
                &chain,
                deposit_req(PathBuf::from("/nonexistent/depositor.json"), 5_000, false),
                mode,
            )
            .await
            .unwrap_err();
            assert_eq!(err.name, "VaultConfigFetchFailed", "{mode:?}");
        }
        let chain = deposit_chain(Pubkey::new_unique(), 0, Pubkey::new_unique());
        let err = deposit(
            &chain,
            deposit_req(PathBuf::from("/nonexistent/depositor.json"), 5_000, false),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(chain.sent_count(), 0);
        assert!(!err.name.is_empty());
    }
}
