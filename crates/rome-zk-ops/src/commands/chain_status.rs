//! `chain-status`: what the chain's settlement accounts say about its health, as `key=value` lines. Read-only and
//! strict: a failed read is a named refusal, never a missing account, because `rollup check` and `rollup status`
//! turn these lines into pass and fail.
//!
//! The lines: the chain's authority, the current slot, the batch heads and posted count, when the chain can be
//! reclaimed and how many slots are left, and the state of its verification keys (how many entries, whether one
//! for the proving layout is active now, and the slot a pending one activates at).

use crate::chain::Chain;
use crate::commands::{read, read_slot, Read};
use crate::error::{Mode, OpsError, Report};
use solana_program::pubkey::Pubkey;

pub async fn run<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    chain_id: u64,
) -> Result<Report, OpsError> {
    let mode = Mode::Confirm; // strict reads
    let mut report = Report::default();

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
        Read::Found(d) => zk_settlement_client::decode_root_account(&d).map_err(|e| {
            OpsError::chain(
                "RootUndecodable",
                format!("the root account {root_pda} does not decode: {e}"),
            )
        })?,
        _ => {
            return Err(OpsError::chain(
                "ChainNotRegistered",
                format!(
                    "chain {chain_id} has no root account {root_pda} under this settlement program"
                ),
            ))
        }
    };

    let (cc_pda, _) = zk_settlement_client::chain_config_pda(settlement, chain_id);
    let cfg = match read(
        chain,
        mode,
        &cc_pda,
        "the chain_config account",
        "ChainConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => zk_settlement_client::decode_chain_config_account(&d).map_err(|e| {
            OpsError::chain(
                "ChainConfigUndecodable",
                format!("the chain_config account {cc_pda} does not decode: {e}"),
            )
        })?,
        _ => {
            return Err(OpsError::chain(
                "ChainConfigMissing",
                format!("chain {chain_id} has no chain_config account {cc_pda}; a chain that predates it needs `migrate` first"),
            ))
        }
    };

    let (gc_pda, _) = zk_settlement_client::global_config_pda(settlement);
    let global = match read(
        chain,
        mode,
        &gc_pda,
        "the global_config account",
        "GlobalConfigLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => zk_settlement_client::decode_global_config_account(&d).map_err(|e| {
            OpsError::chain(
                "GlobalConfigUndecodable",
                format!("the global_config account {gc_pda} does not decode: {e}"),
            )
        })?,
        _ => {
            return Err(OpsError::chain(
                "GlobalConfigMissing",
                format!("the settlement program has no global_config account {gc_pda}"),
            ))
        }
    };

    // A registry account that does not exist is a chain with no keys at all, which is a normal state right after
    // registration; a registry that cannot be read is not.
    let (reg_pda, _) = zk_settlement_client::registry_pda(settlement, chain_id);
    let registry = match read(
        chain,
        mode,
        &reg_pda,
        "the verifier registry account",
        "RegistryLookupFailed",
        &mut report,
    )
    .await?
    {
        Read::Found(d) => Some(
            zk_settlement_client::decode_registry_account(&d).map_err(|e| {
                OpsError::chain(
                    "RegistryUndecodable",
                    format!("the registry account {reg_pda} does not decode: {e}"),
                )
            })?,
        ),
        _ => None,
    };

    let slot = read_slot(chain, mode, &mut report).await?.unwrap_or(0);

    let never_posted = root.head_pending_batch == 0 && root.head_final_batch == 0;
    let deadline = cfg
        .registered_slot
        .saturating_add(global.reclaim_window_slots);
    report.line(format!("chain_id={chain_id}"));
    report.line(format!("authority={}", root.authority));
    report.line(format!("slot={slot}"));
    report.line(format!("head_pending_batch={}", root.head_pending_batch));
    report.line(format!("head_final_batch={}", root.head_final_batch));
    report.line(format!("posted_batches={}", cfg.posted_batches));
    report.line(format!("registered_slot={}", cfg.registered_slot));
    report.line(format!(
        "reclaim_window_slots={}",
        global.reclaim_window_slots
    ));
    report.line(format!("reclaim_deadline_slot={deadline}"));
    if never_posted {
        report.line(format!(
            "reclaim_slots_left={}",
            deadline.saturating_sub(slot)
        ));
    } else {
        report.line("reclaim_slots_left=none");
    }
    report.line(format!("deposit_lamports={}", cfg.deposit_lamports));
    report.line(format!("deposit_refunded={}", cfg.deposit_refunded));

    let entries = registry
        .as_ref()
        .map(|r| r.entries.as_slice())
        .unwrap_or(&[]);
    // The same lines `vkey show` prints, from the same function.
    report
        .lines
        .extend(crate::commands::vkey::summary_lines(entries, slot));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    use rome_zk_layouts::global_config::{self, GlobalConfigFields};
    use rome_zk_layouts::registry::{self, RegistryEntry};

    fn authority() -> Pubkey {
        Pubkey::new_from_array([4u8; 32])
    }

    fn global_bytes(window: u64) -> Vec<u8> {
        let mut d = vec![0u8; global_config::LEN];
        global_config::write(
            &mut d,
            &GlobalConfigFields {
                registry_authority: [1; 32],
                treasury: [2; 32],
                permissionless_init_enabled: true,
                reclaim_window_slots: window,
                deposit_lamports: 5_000_000_000,
                default_fee_base_lamports: 0,
                default_fee_bps: 0,
                pending_registry_authority: [0; 32],
            },
        );
        d
    }

    fn registry_bytes(chain_id: u64, entries: &[(u8, u64)]) -> Vec<u8> {
        let mut d = vec![0u8; registry::REGISTRY_LEN_V2];
        d[registry::OFF_MAGIC..registry::OFF_MAGIC + 4]
            .copy_from_slice(&registry::MAGIC.to_le_bytes());
        d[registry::OFF_CHAIN_ID..registry::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[registry::OFF_COUNT] = entries.len() as u8;
        for (i, (layout, activation)) in entries.iter().enumerate() {
            registry::write_entry(
                &mut d,
                i,
                &RegistryEntry {
                    curve: registry::CURVE_BN254,
                    scheme: registry::SCHEME_ZISK_1_3_1,
                    vkey_hash: [i as u8 + 1; 32],
                    layout_id: *layout,
                },
                *activation,
            )
            .unwrap();
        }
        d
    }

    /// A registered chain: registered at slot 5, a 6_480_000 slot window, the given heads and registry.
    fn world(head_pending: u64, head_final: u64, entries: Option<&[(u8, u64)]>) -> FakeChain {
        let (rt, _) = zk_settlement_client::root_pda(&program(), 77);
        let (cc, _) = zk_settlement_client::chain_config_pda(&program(), 77);
        let (gc, _) = zk_settlement_client::global_config_pda(&program());
        let (rg, _) = zk_settlement_client::registry_pda(&program(), 77);
        let mut chain = FakeChain::default()
            .with(rt, root_bytes(77, authority(), head_pending, head_final))
            .with(
                cc,
                chain_config_bytes(77, 5_000_000_000, false, 0, Some(60)),
            )
            .with(gc, global_bytes(6_480_000));
        if let Some(e) = entries {
            chain = chain.with(rg, registry_bytes(77, e));
        }
        chain
    }

    fn lines(r: &Report) -> String {
        r.lines.join("\n")
    }

    #[tokio::test]
    async fn a_chain_that_never_posted_counts_its_reclaim_slots_down() {
        let chain = world(0, 0, None);
        let r = run(&chain, &program(), 77).await.unwrap();
        let t = lines(&r);
        assert!(t.contains("reclaim_deadline_slot=6480005"), "{t}");
        assert!(t.contains("reclaim_slots_left=6479005"), "{t}"); // slot 1000
        assert!(t.contains(&format!("authority={}", authority())), "{t}");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_chain_that_posted_has_no_reclaim_deadline() {
        let chain = world(1, 0, None);
        let t = lines(&run(&chain, &program(), 77).await.unwrap());
        assert!(t.contains("reclaim_slots_left=none"), "{t}");
    }

    #[tokio::test]
    async fn no_registry_account_means_no_active_key() {
        let chain = world(0, 0, None);
        let t = lines(&run(&chain, &program(), 77).await.unwrap());
        assert!(t.contains("vkey_entries=0"), "{t}");
        assert!(t.contains("vkey_active=no"), "{t}");
        assert!(!t.contains("vkey_pending_activation_slot"), "{t}");
    }

    #[tokio::test]
    async fn an_entry_whose_slot_has_passed_is_active() {
        let chain = world(0, 0, Some(&[(registry::LAYOUT_ZISK_V1, 900)]));
        let t = lines(&run(&chain, &program(), 77).await.unwrap());
        assert!(t.contains("vkey_entries=1"), "{t}");
        assert!(t.contains("vkey_active=yes"), "{t}");
    }

    #[tokio::test]
    async fn an_entry_for_a_future_slot_is_pending_and_says_when() {
        let chain = world(0, 0, Some(&[(registry::LAYOUT_ZISK_V1, 5_000)]));
        let t = lines(&run(&chain, &program(), 77).await.unwrap());
        assert!(t.contains("vkey_active=no"), "{t}");
        assert!(t.contains("vkey_pending_activation_slot=5000"), "{t}");
    }

    #[tokio::test]
    async fn a_retired_entry_and_a_fallback_layout_do_not_count() {
        let chain = world(
            0,
            0,
            Some(&[
                (registry::LAYOUT_ZISK_V1, u64::MAX),
                (registry::LAYOUT_HEADER_FALLBACK, 0),
            ]),
        );
        let t = lines(&run(&chain, &program(), 77).await.unwrap());
        assert!(t.contains("vkey_entries=2"), "{t}");
        assert!(t.contains("vkey_active=no"), "{t}");
    }

    #[tokio::test]
    async fn an_unregistered_chain_is_refused_by_name() {
        let chain = FakeChain::default();
        let err = run(&chain, &program(), 77).await.unwrap_err();
        assert_eq!(err.name, "ChainNotRegistered");
        assert_eq!(err.exit_code, 1);
    }

    #[tokio::test]
    async fn a_failed_read_is_never_a_missing_account() {
        let (rg, _) = zk_settlement_client::registry_pda(&program(), 77);
        let chain = world(0, 0, None).failing(rg, "429 Too Many Requests");
        let err = run(&chain, &program(), 77).await.unwrap_err();
        assert_eq!(err.name, "RegistryLookupFailed");
        assert!(err.detail.contains("429"), "{err}");
    }
}
