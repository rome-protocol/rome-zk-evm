//! `exit-config propose`, `exit-config activate` and `exit-config show`: the chain authority proposes exit-portal,
//! bridge-program, exit-cap and poster-bond values with an activation slot, anyone activates the proposal once
//! that slot has passed, and `show` prints what is in effect and what is pending.

use crate::chain::Chain;
use crate::commands::{execute, read, read_slot, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use rome_zk_layouts::exit::exit_config::{
    PENDING_MASK_BOND, PENDING_MASK_BRIDGE, PENDING_MASK_CAP, PENDING_MASK_PORTAL,
};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use zk_settlement_client::ops_plan::{pending_mask, pending_mask_names};

#[derive(Debug, Clone)]
pub struct ProposeRequest {
    pub settlement: Pubkey,
    pub chain_id: u64,
    pub chain_authority_keypair: PathBuf,
    pub payer_keypair: PathBuf,
    pub exit_portal: Option<[u8; 20]>,
    pub bridge_program: Option<Pubkey>,
    pub exit_cap: Option<u64>,
    pub poster_bond: Option<u64>,
    pub activation_slot: Option<u64>,
    pub activation_delay_slots: Option<u64>,
}

pub async fn propose<C: Chain>(
    chain: &C,
    req: ProposeRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    // Mirror the program's own `InvalidArgument` refusal locally, before any key is read or any transaction
    // is built: an all-None proposal writes an inert exit_config that ActivateExitConfig can only refuse.
    if req.exit_portal.is_none()
        && req.bridge_program.is_none()
        && req.exit_cap.is_none()
        && req.poster_bond.is_none()
    {
        return Err(OpsError::usage(
            "NothingToPropose",
            "refusing: propose-exit-config needs at least one of --exit-portal / --bridge-program / --exit-cap / --poster-bond",
        ));
    }
    if req.activation_slot.is_some() && req.activation_delay_slots.is_some() {
        return Err(OpsError::usage(
            "ActivationFlagsConflict",
            "pass exactly one of --activation-slot or --activation-delay-slots",
        ));
    }
    if req.activation_slot.is_none() && req.activation_delay_slots.is_none() {
        return Err(OpsError::usage(
            "ActivationFlagsMissing",
            "propose needs --activation-slot or --activation-delay-slots",
        ));
    }
    let authority = keys::load(&req.chain_authority_keypair, "--chain-authority-keypair")?;
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let authority_pk = keys::pubkey(&authority);

    let mut report = Report::default();
    let (root_pda, _) = zk_settlement_client::root_pda(&req.settlement, req.chain_id);
    match read(
        chain,
        mode,
        &root_pda,
        "the root account",
        "RootLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            let root = zk_settlement_client::decode_root_account(&d).map_err(|e| {
                OpsError::chain(
                    "RootUndecodable",
                    format!("the root account {root_pda} does not decode: {e}"),
                )
            })?;
            if root.authority != authority_pk {
                return Err(OpsError::chain(
                    "WrongChainAuthority",
                    format!(
                        "--chain-authority-keypair is {authority_pk}, but chain {} belongs to {}",
                        req.chain_id, root.authority
                    ),
                ));
            }
        }
        Read::Missing => {
            return Err(OpsError::chain(
                "ChainNotRegistered",
                format!(
                    "chain {} has no root account {root_pda} under this settlement program",
                    req.chain_id
                ),
            ))
        }
        Read::Unavailable => {}
    }

    let activation_slot = match (req.activation_slot, req.activation_delay_slots) {
        (Some(slot), _) => slot,
        (None, Some(delay)) => {
            let current = match read_slot(chain, mode, &mut report).await? {
                Some(s) => s,
                None => {
                    // Only a dry run gets here. The assumption is printed, never silently applied.
                    report.line("(dry run: current_slot assumed 0, no live cluster to read)");
                    0
                }
            };
            current.saturating_add(delay)
        }
        (None, None) => unreachable!("checked above"),
    };

    let ix = zk_settlement_client::propose_exit_config_ix(
        &req.settlement,
        &authority_pk,
        &keys::pubkey(&payer),
        req.chain_id,
        req.exit_portal,
        req.bridge_program,
        req.exit_cap,
        req.poster_bond,
        activation_slot,
    );
    let mask = pending_mask(
        req.exit_portal,
        req.bridge_program,
        req.exit_cap,
        req.poster_bond,
    );
    let signers = Signers::new(payer, vec![authority]);
    let (exit_config, _) = zk_settlement_client::exit_config_pda(&req.settlement, req.chain_id);
    match execute(chain, mode, "ProposeExitConfig", ix, &signers, &mut report).await? {
        Some(sig) => report.line(format!(
            "ProposeExitConfig: chain {} exit_config {exit_config} pending_mask {mask} ({}) activation_slot {activation_slot}, sig {sig}",
            req.chain_id,
            pending_mask_names(mask)
        )),
        None => report.line(format!(
            "  pending_mask on activation  {mask} ({})  activation_slot {activation_slot}",
            pending_mask_names(mask)
        )),
    }
    Ok(report)
}

#[derive(Debug, Clone)]
pub struct ActivateRequest {
    pub settlement: Pubkey,
    pub chain_id: u64,
    pub payer_keypair: PathBuf,
}

pub async fn activate<C: Chain>(
    chain: &C,
    req: ActivateRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let mut report = Report::default();
    let chain_id = req.chain_id;
    let (exit_config, _) = zk_settlement_client::exit_config_pda(&req.settlement, chain_id);
    match read(
        chain,
        mode,
        &exit_config,
        "the exit_config account",
        "ExitConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            let cfg = zk_settlement_client::decode_exit_config_account(&d).map_err(|e| {
                OpsError::chain(
                    "ExitConfigUndecodable",
                    format!("the exit_config account does not decode: {e}"),
                )
            })?;
            if cfg.pending_mask == 0 {
                return Err(OpsError::chain(
                    "NoPendingExitConfig",
                    format!("no pending exit config for chain {chain_id}: nothing to activate"),
                ));
            }
            if let Some(now) = read_slot(chain, mode, &mut report).await? {
                if now < cfg.activation_slot {
                    return Err(OpsError::chain(
                        "ActivationNotReached",
                        format!(
                            "not yet: {} slots to go (current slot {now}, activation_slot {})",
                            cfg.activation_slot - now,
                            cfg.activation_slot
                        ),
                    ));
                }
            }
        }
        Read::Missing => {
            return Err(OpsError::chain(
                "NoExitConfig",
                format!("no exit config proposed for chain {chain_id}: nothing to activate"),
            ))
        }
        Read::Unavailable => {}
    }
    let ix = zk_settlement_client::activate_exit_config_ix(&req.settlement, chain_id);
    let signers = Signers::new(payer, vec![]);
    if let Some(sig) = execute(chain, mode, "ActivateExitConfig", ix, &signers, &mut report).await?
    {
        report.line(format!("ActivateExitConfig: chain {chain_id}, sig {sig}"));
    }
    Ok(report)
}

/// Read-only. Every failed read is a named refusal: an unreachable RPC is never printed as "no exit config".
pub async fn show<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    chain_id: u64,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let mode = Mode::Confirm; // strict reads
    let (root_pda, _) = zk_settlement_client::root_pda(settlement, chain_id);
    let root = match read(
        chain,
        mode,
        &root_pda,
        "the root account",
        "RootLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => Some(zk_settlement_client::decode_root_account(&d).map_err(|e| {
            OpsError::chain(
                "RootUndecodable",
                format!("the root account {root_pda} does not decode: {e}"),
            )
        })?),
        _ => None,
    };
    let (ec_pda, _) = zk_settlement_client::exit_config_pda(settlement, chain_id);
    let cfg = match read(
        chain,
        mode,
        &ec_pda,
        "the exit_config account",
        "ExitConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => zk_settlement_client::decode_exit_config_account(&d).map_err(|e| {
            OpsError::chain(
                "ExitConfigUndecodable",
                format!("the exit_config account does not decode: {e}"),
            )
        })?,
        _ => {
            report.line(format!(
                "exit_config {ec_pda} (chain {chain_id}): no exit config (exits disabled)"
            ));
            return Ok(report);
        }
    };
    let now = read_slot(chain, mode, &mut report).await?.unwrap_or(0);
    report.line(format!("exit_config {ec_pda} (chain {chain_id}):"));
    report.line(format!(
        "  exit_portal (current)      0x{}",
        hex::encode(cfg.exit_portal)
    ));
    report.line(format!(
        "  bridge_program (current)   {}",
        cfg.bridge_program
    ));
    match &root {
        Some(r) => {
            report.line(format!(
                "  exit_cap_per_window (current, from root)  {}",
                r.exit_cap_per_window
            ));
            report.line(format!(
                "  poster_bond (current, from root)          {}",
                r.poster_bond
            ));
            report.line(format!(
                "  challenge_window_slots (from root)        {}",
                r.challenge_window_slots
            ));
        }
        None => report.line(format!(
            "  root {root_pda} not found, cannot read current cap/bond"
        )),
    }
    report.line(format!(
        "  pending_mask                {} ({})",
        cfg.pending_mask,
        pending_mask_names(cfg.pending_mask)
    ));
    if cfg.pending_mask & PENDING_MASK_PORTAL != 0 {
        report.line(format!(
            "  pending_exit_portal         0x{}",
            hex::encode(cfg.pending_exit_portal)
        ));
    }
    if cfg.pending_mask & PENDING_MASK_BRIDGE != 0 {
        report.line(format!(
            "  pending_bridge_program      {}",
            cfg.pending_bridge_program
        ));
    }
    if cfg.pending_mask & PENDING_MASK_CAP != 0 {
        report.line(format!(
            "  pending_exit_cap            {}",
            cfg.pending_exit_cap
        ));
    }
    if cfg.pending_mask & PENDING_MASK_BOND != 0 {
        report.line(format!(
            "  pending_poster_bond         {}",
            cfg.pending_poster_bond
        ));
    }
    if cfg.pending_mask == 0 {
        report.line("  activation_slot             n/a (nothing pending)");
    } else if now >= cfg.activation_slot {
        report.line(format!(
            "  activation_slot             {} - reached (current slot {now}); activatable now",
            cfg.activation_slot
        ));
    } else {
        report.line(format!(
            "  activation_slot             {} - not yet (current slot {now}, {} slots to go)",
            cfg.activation_slot,
            cfg.activation_slot - now
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    use solana_keypair::Keypair;

    struct World {
        chain: FakeChain,
        authority_path: PathBuf,
        payer_path: PathBuf,
    }

    impl World {
        fn new(tag: &str) -> (Self, Keypair) {
            let (authority, authority_path) = key_file(&format!("{tag}-a"));
            let (_, payer_path) = key_file(&format!("{tag}-p"));
            let (root, _) = zk_settlement_client::root_pda(&program(), 9);
            let chain =
                FakeChain::default().with(root, root_bytes(9, key_pubkey(&authority), 0, 0));
            (
                Self {
                    chain,
                    authority_path,
                    payer_path,
                },
                authority,
            )
        }
        fn propose_req(&self) -> ProposeRequest {
            ProposeRequest {
                settlement: program(),
                chain_id: 9,
                chain_authority_keypair: self.authority_path.clone(),
                payer_keypair: self.payer_path.clone(),
                exit_portal: Some([0x11; 20]),
                bridge_program: None,
                exit_cap: Some(1_000),
                poster_bond: None,
                activation_slot: Some(5_000),
                activation_delay_slots: None,
            }
        }
        fn clean(&self) {
            remove(&self.authority_path);
            remove(&self.payer_path);
        }
    }

    #[tokio::test]
    async fn propose_dry_run_prints_discriminant_26_and_the_portal_and_sends_nothing() {
        let (w, _) = World::new("p1");
        let report = propose(&w.chain, w.propose_req(), Mode::Dry).await.unwrap();
        let text = report.lines.join("\n");
        assert_eq!(w.chain.sent_count(), 0);
        assert!(text.contains("discriminant   26"), "{text}");
        assert!(text.contains(&"11".repeat(20)), "{text}");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
        assert!(
            text.contains("pending_mask on activation  5 (portal,cap)"),
            "{text}"
        );
        w.clean();
    }

    #[tokio::test]
    async fn propose_confirm_sends_one_transaction() {
        let (w, _) = World::new("p2");
        let report = propose(&w.chain, w.propose_req(), Mode::Confirm)
            .await
            .unwrap();
        assert_eq!(w.chain.sent_count(), 1);
        assert!(
            report
                .lines
                .last()
                .unwrap()
                .contains("sig FakeSignature1111"),
            "{:?}",
            report.lines
        );
        w.clean();
    }

    #[tokio::test]
    async fn an_all_empty_proposal_is_refused_by_name_before_anything() {
        let (w, _) = World::new("p3");
        let mut r = w.propose_req();
        r.exit_portal = None;
        r.exit_cap = None;
        let err = propose(&w.chain, r, Mode::Dry).await.unwrap_err();
        assert_eq!(err.name, "NothingToPropose");
        assert!(
            err.to_string()
                .contains("refusing: propose-exit-config needs at least one of"),
            "{err}"
        );
        assert_eq!(err.exit_code, 2);
        w.clean();
    }

    #[tokio::test]
    async fn activation_flags_must_be_exactly_one() {
        let (w, _) = World::new("p4");
        let mut both = w.propose_req();
        both.activation_delay_slots = Some(10);
        assert_eq!(
            propose(&w.chain, both, Mode::Dry).await.unwrap_err().name,
            "ActivationFlagsConflict"
        );
        let mut neither = w.propose_req();
        neither.activation_slot = None;
        assert_eq!(
            propose(&w.chain, neither, Mode::Dry)
                .await
                .unwrap_err()
                .name,
            "ActivationFlagsMissing"
        );
        w.clean();
    }

    #[tokio::test]
    async fn a_key_that_is_not_the_chain_authority_is_refused_by_name() {
        let (w, _) = World::new("p5");
        let (_, other) = key_file("p5-other");
        let mut r = w.propose_req();
        r.chain_authority_keypair = other.clone();
        let err = propose(&w.chain, r, Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "WrongChainAuthority");
        assert_eq!(w.chain.sent_count(), 0);
        remove(&other);
        w.clean();
    }

    #[tokio::test]
    async fn an_unregistered_chain_is_refused_by_name() {
        let (mut w, _) = World::new("p6");
        w.chain = FakeChain::default();
        let err = propose(&w.chain, w.propose_req(), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ChainNotRegistered");
        w.clean();
    }

    #[tokio::test]
    async fn a_delay_is_added_to_the_current_slot() {
        let (w, _) = World::new("p7");
        let mut r = w.propose_req();
        r.activation_slot = None;
        r.activation_delay_slots = Some(172_800);
        let report = propose(&w.chain, r, Mode::Confirm).await.unwrap();
        let slot = 1_000 + 172_800;
        assert!(
            report
                .lines
                .last()
                .unwrap()
                .contains(&format!("activation_slot {slot}")),
            "{:?}",
            report.lines
        );
        w.clean();
    }

    #[tokio::test]
    async fn a_dry_run_with_no_rpc_still_builds_and_says_it_assumed_slot_zero() {
        let (w, _) = World::new("p8");
        let mut r = w.propose_req();
        r.activation_slot = None;
        r.activation_delay_slots = Some(50);
        let report = propose(&crate::chain::OfflineChain, r, Mode::Dry)
            .await
            .unwrap();
        let text = report.lines.join("\n");
        assert!(text.contains("current_slot assumed 0"), "{text}");
        assert!(text.contains("discriminant   26"), "{text}");
        assert!(text.contains("activation_slot 50"), "{text}");
        w.clean();
    }

    fn activate_req(w: &World) -> ActivateRequest {
        ActivateRequest {
            settlement: program(),
            chain_id: 9,
            payer_keypair: w.payer_path.clone(),
        }
    }

    #[tokio::test]
    async fn activate_dry_run_prints_discriminant_27_and_sends_nothing() {
        let (mut w, _) = World::new("a1");
        let (ec, _) = zk_settlement_client::exit_config_pda(&program(), 9);
        w.chain = w.chain.with(ec, exit_config_bytes(9, 5, 900));
        let report = activate(&w.chain, activate_req(&w), Mode::Dry)
            .await
            .unwrap();
        assert!(report.lines.join("\n").contains("discriminant   27"));
        assert_eq!(w.chain.sent_count(), 0);
        w.clean();
    }

    #[tokio::test]
    async fn activate_confirm_sends_once_when_the_slot_has_passed() {
        let (mut w, _) = World::new("a2");
        let (ec, _) = zk_settlement_client::exit_config_pda(&program(), 9);
        w.chain = w.chain.with(ec, exit_config_bytes(9, 5, 900));
        activate(&w.chain, activate_req(&w), Mode::Confirm)
            .await
            .unwrap();
        assert_eq!(w.chain.sent_count(), 1);
        w.clean();
    }

    #[tokio::test]
    async fn activate_refuses_by_name_when_nothing_is_proposed_nothing_is_pending_or_it_is_too_early(
    ) {
        let (mut w, _) = World::new("a3");
        let err = activate(&w.chain, activate_req(&w), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "NoExitConfig");

        let (ec, _) = zk_settlement_client::exit_config_pda(&program(), 9);
        w.chain = FakeChain::default().with(ec, exit_config_bytes(9, 0, 0));
        let err = activate(&w.chain, activate_req(&w), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "NoPendingExitConfig");

        w.chain = FakeChain::default().with(ec, exit_config_bytes(9, 5, 5_000));
        let err = activate(&w.chain, activate_req(&w), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "ActivationNotReached");
        assert!(err.detail.contains("4000 slots to go"), "{err}");
        assert_eq!(w.chain.sent_count(), 0);
        w.clean();
    }

    #[tokio::test]
    async fn show_prints_the_pending_proposal_and_whether_it_is_due() {
        let (mut w, _) = World::new("s1");
        let (ec, _) = zk_settlement_client::exit_config_pda(&program(), 9);
        w.chain = w.chain.with(ec, exit_config_bytes(9, 5, 5_000));
        let report = show(&w.chain, &program(), 9).await.unwrap();
        let text = report.lines.join("\n");
        assert!(
            text.contains("pending_mask                5 (portal,cap)"),
            "{text}"
        );
        assert!(
            text.contains("not yet (current slot 1000, 4000 slots to go)"),
            "{text}"
        );
        assert_eq!(w.chain.sent_count(), 0);
        w.clean();
    }

    #[tokio::test]
    async fn show_never_reads_a_failed_request_as_no_exit_config() {
        let (ec, _) = zk_settlement_client::exit_config_pda(&program(), 9);
        let chain = FakeChain::default().failing(ec, "timeout");
        let err = show(&chain, &program(), 9).await.unwrap_err();
        assert_eq!(err.name, "ExitConfigLookupFailed");
        let none = FakeChain::default();
        let report = show(&none, &program(), 9).await.unwrap();
        assert!(
            report.lines[0].contains("no exit config (exits disabled)"),
            "{:?}",
            report.lines
        );
    }
}
