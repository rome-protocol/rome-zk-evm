//! `bridge-config init` and `bridge-config show`: the bridge's one-time config, which names the settlement program
//! and the inbox program every deposit queue is bound to. The bridge program's upgrade authority runs `init` once
//! per bridge deployment; `show` prints what is recorded, as `key=value` lines.

use crate::chain::Chain;
use crate::commands::{execute_many, read, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use rome_zk_layouts::deposit_queue::bridge_config;
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct InitRequest {
    pub bridge: Pubkey,
    pub settlement: Pubkey,
    pub inbox: Pubkey,
    /// The bridge program's upgrade authority. It signs and pays.
    pub authority_keypair: PathBuf,
}

fn decode(d: &[u8], pda: &Pubkey) -> Result<bridge_config::BridgeConfigFields, OpsError> {
    bridge_config::read(d).map_err(|e| {
        OpsError::chain(
            "BridgeConfigUndecodable",
            format!("bridge_config {pda} does not decode: {e:?}"),
        )
    })
}

fn print_config(report: &mut Report, c: &bridge_config::BridgeConfigFields) {
    report.line(format!(
        "settlement_program={}",
        Pubkey::new_from_array(c.settlement_program)
    ));
    report.line(format!(
        "inbox_program={}",
        Pubkey::new_from_array(c.inbox_program)
    ));
}

/// The upgrade authority a `ProgramData` account records: `Some` when the program can still be upgraded.
/// `ProgramData` is a 4-byte state tag (3), the 8-byte slot, then an optional authority (a 1-byte flag and 32 bytes).
fn upgrade_authority(d: &[u8]) -> Option<Option<Pubkey>> {
    if d.len() < 45 || u32::from_le_bytes(d[0..4].try_into().ok()?) != 3 {
        return None;
    }
    Some((d[12] == 1).then(|| Pubkey::new_from_array(d[13..45].try_into().unwrap())))
}

/// `bridge-config init`: write the config. A config that exists is reported and nothing is sent.
pub async fn init<C: Chain>(chain: &C, req: InitRequest, mode: Mode) -> Result<Report, OpsError> {
    if req.settlement == Pubkey::default() || req.inbox == Pubkey::default() {
        return Err(OpsError::usage(
            "BridgeConfigProgramZero",
            "--settlement and --inbox must both be real program ids",
        ));
    }
    let authority = keys::load(&req.authority_keypair, "--authority-keypair")?;
    let authority_key = keys::pubkey(&authority);
    let mut report = Report::default();
    let (pda, _) = zk_bridge_client::bridge_config_pda(&req.bridge);
    if let Read::Found(d) = read(
        chain,
        mode,
        &pda,
        "the bridge config",
        "BridgeConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        let c = decode(&d, &pda)?;
        report.line(format!(
            "bridge_config {pda} already initialized, nothing sent (it is written once and cannot change):"
        ));
        print_config(&mut report, &c);
        return Ok(report);
    }

    let pd = zk_bridge_client::bridge_program_data(&req.bridge);
    match read(
        chain,
        mode,
        &pd,
        "the bridge program's ProgramData",
        "ProgramDataLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => match upgrade_authority(&d) {
            Some(Some(a)) if a == authority_key => {}
            Some(Some(a)) => {
                return Err(OpsError::chain(
                    "NotUpgradeAuthority",
                    format!(
                        "--authority-keypair is {authority_key}, but the bridge program's upgrade authority is {a}"
                    ),
                ))
            }
            Some(None) => {
                return Err(OpsError::chain(
                    "NotUpgradeAuthority",
                    "the bridge program has no upgrade authority (it is frozen), so no one can write its config",
                ))
            }
            None => {
                return Err(OpsError::chain(
                    "ProgramDataUndecodable",
                    format!("{pd} is not an upgradeable program's ProgramData account"),
                ))
            }
        },
        Read::Missing => {
            return Err(OpsError::chain(
                "BridgeProgramNotDeployed",
                format!(
                    "no upgradeable program {} on this cluster (ProgramData {pd} not found)",
                    req.bridge
                ),
            ))
        }
        Read::Unavailable => {}
    }

    let ix = zk_bridge_client::init_bridge_config_ix(
        &req.bridge,
        &authority_key,
        &authority_key,
        &req.settlement,
        &req.inbox,
    );
    report.line(format!(
        "-- bridge-config init: bridge {}, settlement {}, inbox {}",
        req.bridge, req.settlement, req.inbox
    ));
    report.line(format!("  bridge_config (not yet created): {pda}"));
    report.line("  instruction: InitBridgeConfig");
    for m in &ix.accounts {
        report.line(format!(
            "      {}{}{}",
            m.pubkey,
            if m.is_writable { "  (writable)" } else { "" },
            if m.is_signer { "  (signer)" } else { "" },
        ));
    }
    let signers = Signers::new(authority, vec![]);
    if let Some(sig) = execute_many(
        chain,
        mode,
        "InitBridgeConfig",
        std::slice::from_ref(&ix),
        &signers,
        &mut report,
    )
    .await?
    {
        report.line(format!("-- sent: {sig}"));
    }
    Ok(report)
}

/// `bridge-config show`: the settlement and inbox programs the bridge is bound to. Reads only.
pub async fn show<C: Chain>(chain: &C, bridge: &Pubkey) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let (pda, _) = zk_bridge_client::bridge_config_pda(bridge);
    report.line(format!("bridge_program={bridge}"));
    report.line(format!("bridge_config={pda}"));
    match read(
        chain,
        Mode::Confirm,
        &pda,
        "the bridge config",
        "BridgeConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => {
            report.line("bridge_config_exists=true");
            print_config(&mut report, &decode(&d, &pda)?);
        }
        _ => report.line("bridge_config_exists=false"),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    fn bridge() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn config_key() -> Pubkey {
        zk_bridge_client::bridge_config_pda(&bridge()).0
    }

    fn program_data_key() -> Pubkey {
        zk_bridge_client::bridge_program_data(&bridge())
    }

    /// A `ProgramData` account: tag 3, a slot, then the optional upgrade authority.
    fn program_data(authority: Option<Pubkey>) -> Vec<u8> {
        let mut d = vec![0u8; 45];
        d[0..4].copy_from_slice(&3u32.to_le_bytes());
        if let Some(a) = authority {
            d[12] = 1;
            d[13..45].copy_from_slice(&a.to_bytes());
        }
        d
    }

    fn req(path: PathBuf) -> InitRequest {
        InitRequest {
            bridge: bridge(),
            settlement: program(),
            inbox: inbox(),
            authority_keypair: path,
        }
    }

    fn config_bytes() -> Vec<u8> {
        let mut d = vec![0u8; bridge_config::LEN];
        bridge_config::write(
            &mut d,
            &bridge_config::BridgeConfigFields {
                settlement_program: program().to_bytes(),
                inbox_program: inbox().to_bytes(),
            },
        );
        d
    }

    #[tokio::test]
    async fn init_refuses_a_zero_program_a_wrong_authority_a_frozen_program_and_a_missing_program()
    {
        let (k, path) = key_file("bc-refuse");
        let me = key_pubkey(&k);
        let chain = FakeChain::default().with(program_data_key(), program_data(Some(me)));
        let zero = InitRequest {
            inbox: Pubkey::default(),
            ..req(path.clone())
        };
        assert_eq!(
            init(&chain, zero, Mode::Confirm).await.unwrap_err().name,
            "BridgeConfigProgramZero"
        );
        let other =
            FakeChain::default().with(program_data_key(), program_data(Some(Pubkey::new_unique())));
        assert_eq!(
            init(&other, req(path.clone()), Mode::Confirm)
                .await
                .unwrap_err()
                .name,
            "NotUpgradeAuthority"
        );
        let frozen = FakeChain::default().with(program_data_key(), program_data(None));
        assert_eq!(
            init(&frozen, req(path.clone()), Mode::Confirm)
                .await
                .unwrap_err()
                .name,
            "NotUpgradeAuthority"
        );
        let junk = FakeChain::default().with(program_data_key(), vec![0u8; 10]);
        assert_eq!(
            init(&junk, req(path.clone()), Mode::Confirm)
                .await
                .unwrap_err()
                .name,
            "ProgramDataUndecodable"
        );
        let none = FakeChain::default();
        assert_eq!(
            init(&none, req(path.clone()), Mode::Confirm)
                .await
                .unwrap_err()
                .name,
            "BridgeProgramNotDeployed"
        );
        remove(&path);
        assert_eq!(
            chain.sent_count() + other.sent_count() + frozen.sent_count(),
            0
        );
    }

    #[tokio::test]
    async fn init_on_an_existing_config_reports_it_and_sends_nothing() {
        let (_, path) = key_file("bc-exists");
        let chain = FakeChain::default().with(config_key(), config_bytes());
        let report = init(&chain, req(path.clone()), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert_eq!(chain.sent_count(), 0);
        let text = report.lines.join("\n");
        assert!(text.contains("already initialized, nothing sent"), "{text}");
        assert!(
            text.contains(&format!("settlement_program={}", program())),
            "{text}"
        );
    }

    #[tokio::test]
    async fn init_dry_run_sends_nothing_and_confirm_sends_the_instruction_the_program_expects() {
        let (k, path) = key_file("bc-send");
        let me = key_pubkey(&k);
        let chain = FakeChain::default().with(program_data_key(), program_data(Some(me)));
        let dry = init(&chain, req(path.clone()), Mode::Dry).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(dry
            .lines
            .join("\n")
            .contains("instruction: InitBridgeConfig"));
        let report = init(&chain, req(path.clone()), Mode::Confirm)
            .await
            .unwrap();
        remove(&path);
        assert!(report.sent());
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(
            chain.sent.lock().unwrap()[0],
            zk_bridge_client::init_bridge_config_ix(&bridge(), &me, &me, &program(), &inbox())
        );
    }

    #[tokio::test]
    async fn show_prints_the_two_programs_or_says_there_is_no_config() {
        let chain = FakeChain::default().with(config_key(), config_bytes());
        let t = show(&chain, &bridge()).await.unwrap().lines.join("\n");
        assert!(t.lines().any(|l| l == "bridge_config_exists=true"), "{t}");
        assert!(
            t.lines()
                .any(|l| l == format!("settlement_program={}", program())),
            "{t}"
        );
        assert!(
            t.lines().any(|l| l == format!("inbox_program={}", inbox())),
            "{t}"
        );
        let t = show(&FakeChain::default(), &bridge())
            .await
            .unwrap()
            .lines
            .join("\n");
        assert!(t.contains("bridge_config_exists=false"), "{t}");
        let failing = FakeChain::default().failing(config_key(), "rpc down");
        assert_eq!(
            show(&failing, &bridge()).await.unwrap_err().name,
            "BridgeConfigLookupFailed"
        );
    }
}
