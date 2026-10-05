//! A confirmed `vkey register` against a real local `solana-test-validator`, and `vkey show` reading it back.
//!
//! The settlement program runs for real: the test sets up its global config the way the public one is set up, registers
//! a chain on the permissionless path, then registers the chain's verification key through `vkey register --confirm`
//! (a V1 transaction through rome-zk-solana-sender) and reads it back with `vkey show`. Only the guest rebuild is
//! faked, because it takes a docker build and the ZisK proving keys; its checks run as in production.
//!
//! The validator listens on loopback only and is killed when the test ends. The test FAILS when
//! `solana-test-validator` is not on PATH or the program build is absent (`cargo build-sbf --arch v3`, then
//! `ROME_ZK_PROGRAMS_DIR`, default `target/deploy` in the workspace), so a green run means it ran. Set
//! `ROME_ZK_SKIP_LOCAL_VALIDATOR=1` to skip it on a machine that has neither. Nothing here reaches a public cluster.

use crate::chain::{Chain, Genesis, RpcChain};
use crate::commands::fake::{key_file, key_pubkey, program, remove};
use crate::commands::register::{self, RegisterRequest};
use crate::commands::vkey::{self, RegisterVkeyRequest};
use crate::error::Mode;
use crate::keys::Signers;
use crate::rebuild::{Rebuilder, Rebuilt};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// The real chain for everything, except block 0 of the EVM chain, which a test has no node for.
struct LocalChain {
    inner: RpcChain,
    genesis: Genesis,
}

impl Chain for LocalChain {
    async fn account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, String> {
        self.inner.account(key).await
    }
    async fn slot(&self) -> Result<u64, String> {
        self.inner.slot().await
    }
    async fn genesis(&self, _evm_rpc: &str) -> Result<Genesis, String> {
        Ok(self.genesis)
    }
    async fn send(&self, ixs: &[Instruction], signers: &Signers) -> Result<String, String> {
        self.inner.send(ixs, signers).await
    }
}

struct Rebuilt0(Rebuilt);

impl Rebuilder for Rebuilt0 {
    fn can_rebuild(&self, _z: &str) -> Result<(), String> {
        Ok(())
    }

    async fn rebuild(
        &self,
        _g: &Path,
        _c: u64,
        _t: &str,
        _e: &str,
        _z: &str,
    ) -> Result<Rebuilt, String> {
        Ok(self.0.clone())
    }
}

struct Validator {
    child: Child,
    dir: PathBuf,
    /// The key files the test wrote; removed even when the test fails.
    files: Vec<PathBuf>,
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
        for f in &self.files {
            remove(f);
        }
    }
}

fn free_port_range() -> u16 {
    // Three ports in a row are needed (rpc, rpc+1 for the websocket, a faucet); ask the OS for one and use the block above it.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p.min(60_000)
}

fn programs_dir() -> PathBuf {
    std::env::var_os("ROME_ZK_PROGRAMS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy"))
}

async fn wait_for<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    secs: u64,
    mut f: impl FnMut() -> F,
) -> T {
    for _ in 0..secs * 4 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

#[tokio::test]
async fn a_confirmed_registration_lands_and_vkey_show_prints_it() {
    let dir = programs_dir();
    if std::env::var_os("ROME_ZK_SKIP_LOCAL_VALIDATOR").is_some_and(|v| v == "1") {
        eprintln!("SKIPPED: ROME_ZK_SKIP_LOCAL_VALIDATOR=1");
        return;
    }
    assert!(
        sh("solana-test-validator", &["--version"]).is_some(),
        "solana-test-validator is not on PATH; run this test on a machine where it is installed, or set ROME_ZK_SKIP_LOCAL_VALIDATOR=1 to skip it"
    );
    assert!(
        dir.join("zk_settlement.so").is_file(),
        "zk_settlement.so is not in {} (cargo build-sbf --arch v3, or set ROME_ZK_PROGRAMS_DIR); set ROME_ZK_SKIP_LOCAL_VALIDATOR=1 to skip this test",
        dir.display()
    );
    let work = std::env::temp_dir().join(format!("rome-zk-ops-vkey-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();

    let (upgrade, upgrade_path) = key_file("upgrade");
    let (registry, registry_path) = key_file("registry");
    let (payer, payer_path) = key_file("payer");
    let (owner, owner_path) = key_file("owner");
    let (treasury, treasury_path) = key_file("treasury");
    let settlement = program();

    let rpc_port = free_port_range();
    let rpc_url = format!("http://127.0.0.1:{rpc_port}");
    let mut args: Vec<String> = vec![
        "--reset".into(),
        "--quiet".into(),
        "--ledger".into(),
        work.join("ledger").display().to_string(),
        "--bind-address".into(),
        "127.0.0.1".into(),
        "--rpc-port".into(),
        rpc_port.to_string(),
        "--faucet-port".into(),
        (rpc_port.wrapping_add(200)).to_string(),
        "--gossip-port".into(),
        (rpc_port.wrapping_add(300)).to_string(),
        "--dynamic-port-range".into(),
        format!(
            "{}-{}",
            rpc_port.wrapping_add(400),
            rpc_port.wrapping_add(460)
        ),
        "--upgradeable-program".into(),
        settlement.to_string(),
        dir.join("zk_settlement.so").display().to_string(),
        key_pubkey(&upgrade).to_string(),
    ];
    args.shrink_to_fit();
    let child = Command::new("solana-test-validator")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("solana-test-validator starts");
    let _validator = Validator {
        child,
        dir: work.clone(),
        files: vec![
            upgrade_path.clone(),
            registry_path.clone(),
            payer_path.clone(),
            owner_path.clone(),
            treasury_path.clone(),
        ],
    };

    let rpc = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.clone());
    wait_for("the validator to answer", 120, || async {
        rpc.get_health().await.ok()
    })
    .await;
    for k in [&upgrade, &registry, &payer, &owner, &treasury] {
        let pk = rome_zk_solana_sender::compat::to_v1_pubkey(&key_pubkey(k));
        rpc.request_airdrop(&pk, 100_000_000_000).await.unwrap();
        wait_for("an airdrop", 60, || async {
            rpc.get_balance(&pk).await.ok().filter(|b| *b > 0)
        })
        .await;
    }

    let chain = RpcChain::new(rpc_url.clone());
    // The settlement program's global config, the way the public one is set up: init, then permissionless on.
    let global = zk_settlement_client::GlobalConfigFields {
        registry_authority: key_pubkey(&registry),
        treasury: key_pubkey(&treasury),
        permissionless_init_enabled: false,
        reclaim_window_slots: 216_000,
        deposit_lamports: 1_000_000,
        default_fee_base_lamports: 0,
        default_fee_bps: 0,
    };
    let init = zk_settlement_client::init_global_config_ix(
        &settlement,
        &key_pubkey(&payer),
        &key_pubkey(&upgrade),
        global,
    );
    chain
        .send(
            &[init],
            &Signers::new(crate::keys::copy(&payer), vec![crate::keys::copy(&upgrade)]),
        )
        .await
        .expect("InitGlobalConfig lands");
    let set = zk_settlement_client::set_global_config_ix(
        &settlement,
        &key_pubkey(&registry),
        zk_settlement_client::GlobalConfigUpdate {
            permissionless_init_enabled: true,
            reclaim_window_slots: 216_000,
            deposit_lamports: 1_000_000,
            default_fee_base_lamports: 0,
            default_fee_bps: 0,
        },
    );
    chain
        .send(
            &[set],
            &Signers::new(
                crate::keys::copy(&payer),
                vec![crate::keys::copy(&registry)],
            ),
        )
        .await
        .expect("SetGlobalConfig lands");

    // A chain: the id the program derives for this owner, a genesis for that id, registered on the permissionless path.
    let chain_id = zk_settlement_client::derive_permissionless_chain_id(&key_pubkey(&owner), 0);
    let genesis_path = work.join("genesis.json");
    std::fs::write(
        &genesis_path,
        crate::genesis::tests::genesis_json(chain_id, "0x0"),
    )
    .unwrap();
    let genesis = crate::genesis::load(&genesis_path).unwrap();
    let local = LocalChain {
        inner: chain,
        genesis: (genesis.block_hash, genesis.state_root),
    };
    let report = register::run(
        &local,
        RegisterRequest {
            keypair: owner_path.clone(),
            settlement,
            inbox: Pubkey::new_unique(),
            evm_rpc: "http://evm.invalid".into(),
            reserved: false,
            chain_id: None,
            registry_keypair: None,
            layout1_vkey_json: None,
            zisk_vkey_json: None,
            nonce: None,
            expect_chain_id: None,
            challenge_window_slots: register::DEFAULT_CHALLENGE_WINDOW_SLOTS,
            max_pending: register::DEFAULT_MAX_PENDING,
            max_drift_secs: register::DEFAULT_MAX_DRIFT_SECS,
        },
        Mode::Confirm,
    )
    .await
    .expect("the chain registers");
    assert!(report.sent(), "{:?}", report.lines);

    // Nothing is registered yet.
    let before = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    assert!(before.contains("vkey_entries=0"), "{before}");

    let elf = "ab12".repeat(16);
    let vk = [0x5a; 32];
    let rebuilt = Rebuilt0(Rebuilt {
        elf_sha256: elf.clone(),
        genesis_sha256: genesis.sha256.clone(),
        chain_id,
        genesis_balance: 0,
        genesis_balance_remainder_wei: 0,
        program_vk: Some(vk),
    });
    let request = RegisterVkeyRequest {
        settlement,
        chain_id,
        genesis: genesis_path.clone(),
        guest_tag: "v0.2.0".into(),
        zisk: "1.3.1-alpha".into(),
        evm_tag: "v0.2.1".into(),
        anchor_evm_tag: None,
        anchor_zisk: None,
        bridge_program: vkey::default_bridge_program(),
        elf_sha256: hex::decode(&elf).unwrap().try_into().unwrap(),
        program_vk: vk,
        genesis_sha256: None,
        registry_keypair: registry_path.clone(),
        payer_keypair: payer_path.clone(),
        activation_slot: None,
        activation_delay_slots: Some(50),
        anchor_guest_tag: None,
        move_pending_activation: false,
    };

    // A wrong ELF is refused against the live chain, and nothing is registered.
    let mut wrong = request.clone();
    wrong.elf_sha256 = [9; 32];
    let e = vkey::register(&local, &rebuilt, wrong, Mode::Confirm)
        .await
        .unwrap_err();
    assert_eq!(e.name, "ElfMismatch", "{e}");
    let still = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    assert!(still.contains("vkey_entries=0"), "{still}");

    // A dry run sends nothing.
    let dry = vkey::register(&local, &rebuilt, request.clone(), Mode::Dry)
        .await
        .unwrap();
    assert!(!dry.sent(), "{:?}", dry.lines);
    let after_dry = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    assert!(after_dry.contains("vkey_entries=0"), "{after_dry}");

    // The confirmed registration lands, and `vkey show` prints it.
    let again_request = request.clone();
    let done = vkey::register(&local, &rebuilt, request, Mode::Confirm)
        .await
        .unwrap();
    assert!(done.sent(), "{:?}", done.lines);
    let shown = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    assert!(shown.contains("vkey_entries=1"), "{shown}");
    assert!(
        shown.contains(&format!("vkey=0x{}", hex::encode(vk))),
        "{shown}"
    );
    assert!(
        shown.contains("state=pending") || shown.contains("state=active"),
        "{shown}"
    );
    assert!(shown.contains("activation_slot="), "{shown}");

    // Sending the same key again is refused by name (pending or active, depending on how far the validator has
    // run), and the registry is unchanged.
    let again = vkey::register(&local, &rebuilt, again_request, Mode::Confirm)
        .await
        .unwrap_err();
    assert!(
        matches!(again.name, "VkeyPending" | "VkeyAlreadyActive"),
        "{again}"
    );
    let unchanged = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    // The state can move from pending to active as the validator runs; the key and its activation slot cannot.
    let entry = |t: &str| {
        let line = t.lines().find(|l| l.starts_with("vkey_entry_0=")).unwrap();
        line[line.find(" vkey=").unwrap()..].to_string()
    };
    assert_eq!(entry(&shown), entry(&unchanged));
    assert!(
        shown.contains("scheme=2 ") && shown.contains("zisk=1.3.1-alpha zisk_status=open"),
        "{shown}"
    );
    println!("{shown}");

    // `vkey retire-version` finds the registry by scanning the program's accounts, lists the entry in a dry run
    // without sending, retires it on --confirm, and has nothing left to retire afterwards.
    let scan = vkey::RpcScan {
        url: rpc_url.clone(),
    };
    let retire = vkey::RetireVersionRequest {
        settlement,
        zisk: "1.3.1-alpha".into(),
        registry_keypair: registry_path.clone(),
        payer_keypair: payer_path.clone(),
    };
    let listed = vkey::retire_version(&local, &scan, retire.clone(), Mode::Dry)
        .await
        .unwrap();
    assert!(!listed.sent(), "{:?}", listed.lines);
    let t = listed.lines.join("\n");
    assert!(t.contains("entries_to_retire=1"), "{t}");
    assert!(
        t.contains(&format!("retire chain_id={chain_id} entry=0")),
        "{t}"
    );
    let gone = vkey::retire_version(&local, &scan, retire.clone(), Mode::Confirm)
        .await
        .unwrap();
    assert!(gone.sent(), "{:?}", gone.lines);
    let after = vkey::show(&local, &settlement, chain_id)
        .await
        .unwrap()
        .lines
        .join("\n");
    assert!(after.contains("vkey_entry_0=state=retired"), "{after}");
    let nothing = vkey::retire_version(&local, &scan, retire, Mode::Confirm)
        .await
        .unwrap();
    assert!(!nothing.sent(), "{:?}", nothing.lines);
    assert!(
        nothing.lines.join("\n").contains("entries_to_retire=0"),
        "{:?}",
        nothing.lines
    );
}
