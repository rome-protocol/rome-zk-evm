//! `register`: register a chain in the settlement program. Two paths, as before.
//!
//! - `--reserved` (`chain_id < 2^32`): the id must already have a live `AllowReservedId` marker, and both the chain
//!   authority and the registry authority sign. Both verifier-key files are required.
//! - permissionless (the default): the program derives the id from the authority's nonce, locks the configured
//!   deposit, and registers the chain with no verifier keys.
//!
//! The flag rules run first, before a key is read or the chain is asked anything.

use crate::chain::Chain;
use crate::commands::{chain_id::read_perm_nonce, chain_id::refusal, execute, read, Read};
use crate::error::{Mode, OpsError, Report};
use crate::keys::{self, Signers};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use zk_settlement_client::ops_plan::plan_permissionless;

/// The default challenge window: 24 h at about 2 slots a second.
pub const DEFAULT_CHALLENGE_WINDOW_SLOTS: u32 = 24 * 60 * 60 * 2;
/// The default pending-batch limit: the window divided by the batch time, plus 10 %.
pub const DEFAULT_MAX_PENDING: u32 = 9_500;
/// The prove window: 4 h.
const PROVE_WINDOW_SLOTS: u32 = 4 * 60 * 60 * 2;
/// The default drift bound written into `chain_config` v2.
pub const DEFAULT_MAX_DRIFT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct RegisterRequest {
    pub keypair: PathBuf,
    pub settlement: Pubkey,
    pub inbox: Pubkey,
    pub evm_rpc: String,
    pub reserved: bool,
    pub chain_id: Option<u64>,
    pub registry_keypair: Option<PathBuf>,
    pub layout1_vkey_json: Option<PathBuf>,
    pub zisk_vkey_json: Option<PathBuf>,
    pub nonce: Option<String>,
    pub expect_chain_id: Option<String>,
    pub challenge_window_slots: u32,
    pub max_pending: u32,
    pub max_drift_secs: u64,
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32], OpsError> {
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|_| OpsError::usage("BadVkeyFile", format!("{what} is not hex")))?;
    bytes
        .try_into()
        .map_err(|_| OpsError::usage("BadVkeyFile", format!("{what} is not 32 bytes")))
}

fn program_vk(path: &std::path::Path) -> Result<[u8; 32], OpsError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        OpsError::usage(
            "BadVkeyFile",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        OpsError::usage(
            "BadVkeyFile",
            format!("{} is not JSON: {e}", path.display()),
        )
    })?;
    let s = v["programVK"].as_str().ok_or_else(|| {
        OpsError::usage(
            "BadVkeyFile",
            format!("{} has no programVK", path.display()),
        )
    })?;
    hex32(s, &format!("programVK in {}", path.display()))
}

pub async fn run<C: Chain>(
    chain: &C,
    req: RegisterRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    // First thing, before any key is read or any RPC is called.
    zk_settlement_client::check_register_chain_flags(
        req.reserved,
        req.layout1_vkey_json.as_deref().and_then(|p| p.to_str()),
        req.zisk_vkey_json.as_deref().and_then(|p| p.to_str()),
    )
    .map_err(|r| {
        let name = match r {
            zk_settlement_client::RegisterChainRefusal::VkeysNotAllowedOnPermissionless => {
                "VkeysNotAllowedOnPermissionless"
            }
            zk_settlement_client::RegisterChainRefusal::MissingLayout1Vkey => "MissingLayout1Vkey",
            zk_settlement_client::RegisterChainRefusal::MissingZiskVkey => "MissingZiskVkey",
        };
        let text = r.to_string();
        let detail = text.strip_prefix(&format!("{name}: ")).unwrap_or(&text);
        OpsError::usage(name, detail.to_string())
    })?;

    let authority = keys::load(&req.keypair, "--keypair")?;
    let authority_pk = keys::pubkey(&authority);
    let recorded = plan_permissionless(
        req.reserved,
        &authority_pk,
        req.nonce.as_deref(),
        req.expect_chain_id.as_deref(),
    )
    .map_err(refusal)?;

    let (block_hash, state_root) = chain
        .genesis(&req.evm_rpc)
        .await
        .map_err(|e| OpsError::chain("GenesisLookupFailed", e))?;

    // Verifier keys only exist on the reserved path; a permissionless chain registers with none.
    let registry_entries = if req.reserved {
        let zisk = program_vk(req.zisk_vkey_json.as_deref().expect("checked above"))?;
        let layout1 = program_vk(req.layout1_vkey_json.as_deref().expect("checked above"))?;
        zk_settlement_client::registry_entries_for_init(layout1, zisk)
    } else {
        vec![]
    };

    let fields = zk_settlement_client::InitChainFields {
        number: 0,
        parent_hash: [0u8; 32],
        state_root,
        block_hash,
        profile: 0, // public
        challenge_window_slots: req.challenge_window_slots,
        prove_window_slots: PROVE_WINDOW_SLOTS,
        proving_policy: 1,      // on-challenge
        poster_bond: 0,         // no bond
        exit_cap_per_window: 0, // 0 = exits off (ProveExit refuses ExitCapUnset) until governance sets a cap
        max_pending: req.max_pending,
        inbox_program: req.inbox,
        max_drift_secs: req.max_drift_secs,
        registry_entries,
    };

    let mut report = Report::default();
    let (ix, signers, chain_id) = if req.reserved {
        let chain_id = req.chain_id.ok_or_else(|| {
            OpsError::usage("ReservedNeedsChainId", "--reserved requires --chain-id")
        })?;
        let registry_path = req.registry_keypair.as_deref().ok_or_else(|| {
            OpsError::usage(
                "ReservedNeedsRegistryKeypair",
                "--reserved requires --registry-keypair",
            )
        })?;
        let registry = keys::load(registry_path, "--registry-keypair")?;
        let (root_pda, _) = zk_settlement_client::root_pda(&req.settlement, chain_id);
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
            Read::Found(_) => {
                report.line(format!(
                    "chain {chain_id} already registered: root {root_pda}"
                ));
                return Ok(report);
            }
            Read::Missing | Read::Unavailable => {}
        }
        let ix = zk_settlement_client::init_chain_reserved_ix(
            &req.settlement,
            &authority_pk,
            &authority_pk,
            &keys::pubkey(&registry),
            chain_id,
            fields,
        );
        (ix, Signers::new(authority, vec![registry]), chain_id)
    } else {
        // The nonce and id the caller recorded are sent as given, so the program refuses a stale pair in this
        // very transaction; without the flags the nonce is read now and the id derived from it.
        let (nonce, chain_id) = match recorded {
            Some(pair) => pair,
            None => {
                let nonce = read_perm_nonce(chain, &req.settlement, &authority_pk).await?;
                (
                    nonce,
                    zk_settlement_client::derive_permissionless_chain_id(&authority_pk, nonce),
                )
            }
        };
        let ix = zk_settlement_client::init_chain_permissionless_ix(
            &req.settlement,
            &authority_pk,
            &authority_pk,
            chain_id,
            nonce,
            fields,
        );
        (ix, Signers::new(authority, vec![]), chain_id)
    };

    let sig = execute(chain, mode, "InitChain", ix, &signers, &mut report).await?;
    let (root_pda, _) = zk_settlement_client::root_pda(&req.settlement, chain_id);
    let path = if req.reserved {
        "reserved"
    } else {
        "permissionless"
    };
    match sig {
        Some(sig) => {
            report.line(format!(
                "chain {chain_id} registered ({path}): root {root_pda}, genesis block {} state_root {}, sig {sig}",
                hex::encode(block_hash),
                hex::encode(state_root)
            ));
            if !req.reserved {
                report.line(format!(
                    "chain {chain_id} registered; it cannot finalize a proved root until Rome registers its layout-1 verifier key (SetRegistryEntry)"
                ));
            }
        }
        None => report.line(format!(
            "  would register chain {chain_id} ({path}): root {root_pda}; pass --confirm to send"
        )),
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    fn req(keypair: PathBuf) -> RegisterRequest {
        RegisterRequest {
            keypair,
            settlement: program(),
            inbox: inbox(),
            evm_rpc: "http://evm.invalid".to_string(),
            reserved: false,
            chain_id: None,
            registry_keypair: None,
            layout1_vkey_json: None,
            zisk_vkey_json: None,
            nonce: None,
            expect_chain_id: None,
            challenge_window_slots: DEFAULT_CHALLENGE_WINDOW_SLOTS,
            max_pending: DEFAULT_MAX_PENDING,
            max_drift_secs: DEFAULT_MAX_DRIFT_SECS,
        }
    }

    fn vkey_file(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rome-zk-ops-vkey-{}-{tag}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!("{{\"programVK\":\"0x{}\"}}", "ab".repeat(32)),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn dry_run_prints_the_transaction_and_sends_nothing() {
        let (_, path) = key_file("reg-dry");
        let chain = FakeChain::default();
        let report = run(&chain, req(path.clone()), Mode::Dry).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(!report.sent());
        let text = report.lines.join("\n");
        assert!(text.contains("nothing sent to any cluster"), "{text}");
        assert!(text.contains("pass --confirm to send"), "{text}");
        remove(&path);
    }

    #[tokio::test]
    async fn confirm_sends_one_init_chain_and_reports_the_signature() {
        let (k, path) = key_file("reg-confirm");
        let chain = FakeChain::default();
        let report = run(&chain, req(path.clone()), Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 1);
        assert_eq!(report.signature.as_deref(), Some("FakeSignature1111"));
        let expected = zk_settlement_client::derive_permissionless_chain_id(&key_pubkey(&k), 0);
        let text = report.lines.join("\n");
        assert!(
            text.starts_with(&format!("chain {expected} registered (permissionless)")),
            "{text}"
        );
        remove(&path);
    }

    #[tokio::test]
    async fn vkey_files_on_the_permissionless_path_are_refused_by_name() {
        let (_, path) = key_file("reg-vk");
        let vk = vkey_file("perm");
        let mut r = req(path.clone());
        r.zisk_vkey_json = Some(vk.clone());
        let chain = FakeChain::default();
        let err = run(&chain, r, Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "VkeysNotAllowedOnPermissionless");
        assert_eq!(err.exit_code, 2);
        assert_eq!(chain.sent_count(), 0);
        remove(&path);
        remove(&vk);
    }

    #[tokio::test]
    async fn reserved_without_the_two_vkey_files_is_refused_by_name() {
        let (_, path) = key_file("reg-res");
        let vk = vkey_file("res");
        let mut r = req(path.clone());
        r.reserved = true;
        let chain = FakeChain::default();
        let err = run(&chain, r.clone(), Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "MissingLayout1Vkey");
        r.layout1_vkey_json = Some(vk.clone());
        let err = run(&chain, r, Mode::Confirm).await.unwrap_err();
        assert_eq!(err.name, "MissingZiskVkey");
        assert_eq!(chain.sent_count(), 0);
        remove(&path);
        remove(&vk);
    }

    #[tokio::test]
    async fn the_recorded_nonce_pair_is_checked_before_anything_is_sent() {
        let (_, path) = key_file("reg-pair");
        let chain = FakeChain::default();

        let mut r = req(path.clone());
        r.expect_chain_id = Some("5".into());
        assert_eq!(
            run(&chain, r, Mode::Confirm).await.unwrap_err().name,
            "ExpectChainIdNeedsNonce"
        );

        let mut r = req(path.clone());
        r.nonce = Some("1".into());
        r.expect_chain_id = Some("5".into());
        assert_eq!(
            run(&chain, r, Mode::Confirm).await.unwrap_err().name,
            "ExpectedChainIdMismatch"
        );

        let mut r = req(path.clone());
        r.nonce = Some("x".into());
        assert_eq!(
            run(&chain, r, Mode::Confirm).await.unwrap_err().name,
            "BadNonce"
        );

        let mut r = req(path.clone());
        r.reserved = true;
        r.nonce = Some("1".into());
        r.layout1_vkey_json = Some(PathBuf::from("a"));
        r.zisk_vkey_json = Some(PathBuf::from("b"));
        assert_eq!(
            run(&chain, r, Mode::Confirm).await.unwrap_err().name,
            "PermissionlessFlagsOnReserved"
        );
        assert_eq!(chain.sent_count(), 0);
        remove(&path);
    }

    #[tokio::test]
    async fn a_failed_nonce_read_is_a_refusal_and_sends_nothing() {
        let (k, path) = key_file("reg-nonce");
        let (pda, _) = zk_settlement_client::perm_nonce_pda(&program(), &key_pubkey(&k));
        let chain = FakeChain::default().failing(pda, "503 Service Unavailable");
        let err = run(&chain, req(path.clone()), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(err.name, "NonceLookupFailed");
        assert_eq!(chain.sent_count(), 0);
        remove(&path);
    }

    #[tokio::test]
    async fn an_unreadable_key_file_is_refused_without_showing_anything() {
        let chain = FakeChain::default();
        let err = run(
            &chain,
            req(PathBuf::from("/nonexistent/key.json")),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(err.name, "KeypairUnreadable");
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_reserved_chain_that_is_already_registered_sends_nothing() {
        let (_, path) = key_file("reg-res-auth");
        let (_, registry_path) = key_file("reg-res-reg");
        let vk = vkey_file("res2");
        let (root, _) = zk_settlement_client::root_pda(&program(), 200_101);
        let chain = FakeChain::default().with(root, vec![1, 2, 3]);
        let mut r = req(path.clone());
        r.reserved = true;
        r.chain_id = Some(200_101);
        r.registry_keypair = Some(registry_path.clone());
        r.layout1_vkey_json = Some(vk.clone());
        r.zisk_vkey_json = Some(vk.clone());
        let report = run(&chain, r, Mode::Confirm).await.unwrap();
        assert_eq!(chain.sent_count(), 0);
        assert!(
            report.lines[0].starts_with("chain 200101 already registered"),
            "{:?}",
            report.lines
        );
        remove(&path);
        remove(&registry_path);
        remove(&vk);
    }

    #[tokio::test]
    async fn a_reserved_registration_is_signed_by_both_authorities() {
        let (_, path) = key_file("reg-res3a");
        let (_, registry_path) = key_file("reg-res3b");
        let vk = vkey_file("res3");
        let chain = FakeChain::default();
        let mut r = req(path.clone());
        r.reserved = true;
        r.chain_id = Some(200_101);
        r.registry_keypair = Some(registry_path.clone());
        r.layout1_vkey_json = Some(vk.clone());
        r.zisk_vkey_json = Some(vk.clone());
        // The dry run builds the V1 transaction with both signers; a missing signer would fail the build.
        let report = run(&chain, r, Mode::Dry).await.unwrap();
        assert!(
            report.lines.join("\n").contains("discriminant"),
            "{:?}",
            report.lines
        );
        remove(&path);
        remove(&registry_path);
        remove(&vk);
    }
}
