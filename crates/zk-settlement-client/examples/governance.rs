//! Governance CLI for a deployed zk-settlement program (registration-and-revenue proposal). The registry
//! subcommands wrap one instruction builder from this crate each; the three exit-config subcommands are served
//! by `rome-zk-ops` (`rome-zk-ops exit-config propose|activate|show`), and this example only forwards to it.
//! Every transaction goes out as a V1 transaction through `rome-zk-solana-sender`. Keys are always read from
//! FILE PATHS and never printed (only pubkeys are).
//!
//! Subcommands:
//!   init-global-config   — one-time bring-up (idempotent: skips if global_config already exists).
//!   allow-reserved-id     — registry-authority-only: allowlist a reserved chain id.
//!   revoke-reserved-id    — registry-authority-only: revoke a reserved chain id's marker.
//!   set-global-config     — registry-authority-only: replace every field of global_config.
//!   propose-registry-authority — the CURRENT registry authority proposes a rotation.
//!   accept-registry-authority  — the PROPOSED key accepts and becomes the registry authority.
//!   set-drift-bound       — registry-authority-only: set a chain's drift bound
//!                            (requires the chain's `chain_config` already be v2).
//!   set-registry-entry     — registry-authority-only: register a verifier key (keyed on
//!                            curve/scheme/vkey_hash) with an explicit activation delay, or update an
//!                            already-registered key's activation slot; `--activation-slot retire`
//!                            retires it immediately.
//!   show                   — decode + print global_config, and chain_config if --chain-id is given.
//!   propose-exit-config    — chain-authority-only: propose new exit-portal/bridge-program/exit-cap/
//!                            poster-bond values with an activation delay. Refuses
//!                            locally (before sending) if none of the four optional fields are given.
//!   activate-exit-config   — permissionless: copy a chain's pending exit-config proposal into effect
//!                            once its activation slot has passed.
//!   show-exit-config       — read-only: print a chain's current + pending exit config, and whether the
//!                            pending proposal (if any) is activatable yet.
//!
//! `--dry-run` (propose-exit-config / activate-exit-config only; handled by rome-zk-ops): build and sign the transaction against
//! an all-zero placeholder blockhash and print it (base64, decoded instruction fields, discriminant)
//! instead of sending — no RPC call of any kind is made, so this needs no live cluster.
//!
//! Usage:
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     init-global-config --settlement <PROGRAM_ID> \
//!     --authority-keypair /path/to/upgrade_authority.json --payer-keypair /path/to/payer.json \
//!     --registry-authority <PUBKEY> --treasury <PUBKEY> \
//!     --reclaim-window-slots 216000 --deposit-lamports 25000000000 \
//!     --fee-base-lamports 1000000 --fee-bps 0 [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     allow-reserved-id --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     revoke-reserved-id --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --registry-keypair /path/to/registry_authority.json [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     set-global-config --settlement <PROGRAM_ID> --registry-keypair /path/to/registry_authority.json \
//!     --permissionless-init-enabled false --reclaim-window-slots 216000 --deposit-lamports 25000000000 \
//!     --fee-base-lamports 1000000 --fee-bps 0 [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     propose-registry-authority --settlement <PROGRAM_ID> \
//!     --registry-keypair /path/to/current_registry_authority.json --new <PUBKEY> [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     accept-registry-authority --settlement <PROGRAM_ID> \
//!     --pending-keypair /path/to/proposed_authority.json [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     set-drift-bound --settlement <PROGRAM_ID> --chain-id 200101 --max-drift-secs 60 \
//!     --registry-keypair /path/to/registry_authority.json [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     set-registry-entry --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --curve 0 --scheme 2 --layout-id 1 \
//!     --vkey-hash 44916015a37f85417e8e5c5bd4c8a56498821d3380b3d90874df471a8d12ca91 \
//!     --activation-slot now \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json [--rpc-url URL]
//!
//!   # Retire that same vkey once its dispute window has passed (or immediately on compromise) --
//!   # curve/scheme/vkey-hash/layout-id must match the entry being retired; --activation-slot switches
//!   # from a slot number to the literal word "retire".
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     set-registry-entry --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --curve 0 --scheme 2 --layout-id 1 \
//!     --vkey-hash 44916015a37f85417e8e5c5bd4c8a56498821d3380b3d90874df471a8d12ca91 \
//!     --activation-slot retire \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     show --settlement <PROGRAM_ID> [--chain-id 200101] [--rpc-url URL]
//!
//!   # exactly one of --activation-slot / --activation-delay-slots; at least one of the four optional
//!   # fields (--exit-portal/--bridge-program/--exit-cap/--poster-bond) or this refuses before sending.
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     propose-exit-config --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --exit-portal 1111111111111111111111111111111111111111 --exit-cap 1000000000 \
//!     --activation-delay-slots 172800 \
//!     --chain-authority-keypair /path/to/chain_authority.json --payer-keypair /path/to/payer.json \
//!     [--dry-run] [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     activate-exit-config --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --payer-keypair /path/to/payer.json [--dry-run] [--rpc-url URL]
//!
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     show-exit-config --settlement <PROGRAM_ID> --chain-id 200101 [--rpc-url URL]

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
};
use std::str::FromStr;

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}
fn required_arg(name: &str) -> String {
    arg(name).unwrap_or_else(|| panic!("{name} is required"))
}
fn pubkey_arg(name: &str) -> Pubkey {
    Pubkey::from_str(&required_arg(name)).unwrap_or_else(|_| panic!("{name}: bad pubkey"))
}
fn u64_arg(name: &str) -> u64 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u64"))
}
fn u32_arg(name: &str) -> u32 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u32"))
}
fn u8_arg(name: &str) -> u8 {
    required_arg(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name}: bad u8"))
}
fn hex32_arg(name: &str) -> [u8; 32] {
    let s = required_arg(name);
    let bytes =
        hex::decode(s.trim_start_matches("0x")).unwrap_or_else(|_| panic!("{name}: not valid hex"));
    bytes
        .try_into()
        .unwrap_or_else(|_| panic!("{name}: expected 32 bytes"))
}
fn bool_arg(name: &str) -> bool {
    match required_arg(name).as_str() {
        "true" => true,
        "false" => false,
        other => panic!("{name}: expected true|false, got {other}"),
    }
}
const EXIT_CONFIG_FORWARDS: &[(&str, &[&str])] = &[
    ("propose-exit-config", &["exit-config", "propose"]),
    ("activate-exit-config", &["exit-config", "activate"]),
    ("show-exit-config", &["exit-config", "show"]),
];

fn rpc_url() -> String {
    arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string())
}
fn settlement_id() -> Pubkey {
    pubkey_arg("--settlement")
}

/// Sends one instruction as one V1 transaction through `rome-zk-solana-sender`, via the operator CLI's chain
/// seam, and waits for it to confirm.
async fn send_ix(
    rpc_url: &str,
    ix: solana_program::instruction::Instruction,
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> String {
    use rome_zk_ops::chain::{Chain, RpcChain};
    let signers = rome_zk_ops::keys::Signers::new(
        rome_zk_ops::keys::copy(payer),
        extra_signers
            .iter()
            .map(|k| rome_zk_ops::keys::copy(k))
            .collect(),
    );
    RpcChain::new(rpc_url.to_string())
        .send(&[ix], &signers)
        .await
        .expect("transaction")
}

async fn print_global_config(rpc: &RpcClient, settlement: &Pubkey) {
    let (global_pda, _) = zk_settlement_client::global_config_pda(settlement);
    match rpc.get_account(&global_pda).await {
        Ok(acc) => {
            let cfg = zk_settlement_client::decode_global_config_account(&acc.data)
                .expect("decode global_config");
            println!("global_config {global_pda}:");
            println!("  registry_authority          {}", cfg.registry_authority);
            println!(
                "  pending_registry_authority  {}{}",
                cfg.pending_registry_authority,
                if cfg.pending_registry_authority == Pubkey::default() {
                    " (none pending)"
                } else {
                    ""
                }
            );
            println!("  treasury                     {}", cfg.treasury);
            println!(
                "  permissionless_init_enabled  {}",
                cfg.permissionless_init_enabled
            );
            println!(
                "  reclaim_window_slots         {}",
                cfg.reclaim_window_slots
            );
            println!("  deposit_lamports             {}", cfg.deposit_lamports);
            println!(
                "  default_fee_base_lamports    {}",
                cfg.default_fee_base_lamports
            );
            println!("  default_fee_bps              {}", cfg.default_fee_bps);
        }
        Err(_) => println!("global_config {global_pda}: not initialized"),
    }
}

async fn print_chain_config(rpc: &RpcClient, settlement: &Pubkey, chain_id: u64) {
    let (cc_pda, _) = zk_settlement_client::chain_config_pda(settlement, chain_id);
    match rpc.get_account(&cc_pda).await {
        Ok(acc) => {
            let cfg = zk_settlement_client::decode_chain_config_account(&acc.data)
                .expect("decode chain_config");
            println!("chain_config {cc_pda} (chain {chain_id}):");
            println!("  reserved            {}", cfg.reserved);
            println!("  deposit_lamports    {}", cfg.deposit_lamports);
            println!("  deposit_refunded    {}", cfg.deposit_refunded);
            println!("  registered_slot     {}", cfg.registered_slot);
            println!("  posted_batches      {}", cfg.posted_batches);
            println!("  fee_base_lamports   {}", cfg.fee_base_lamports);
            println!("  fee_bps             {}", cfg.fee_bps);
            match cfg.max_drift_secs {
                Some(v) => println!("  max_drift_secs      {v} (v2)"),
                None => println!("  max_drift_secs      unset (v1 — needs MigrateChainV2)"),
            }
        }
        Err(_) => println!("chain_config {cc_pda} (chain {chain_id}): not migrated/registered"),
    }
}

#[tokio::main]
async fn main() {
    // The exit-config subcommands live in rome-zk-ops. Forwarded, they keep this example's old behaviour: a
    // send unless `--dry-run` is given.
    if let Some(argv) = rome_zk_ops::cli::example_argv(
        &[],
        EXIT_CONFIG_FORWARDS,
        std::env::args().skip(1).collect(),
    ) {
        std::process::exit(rome_zk_ops::cli::run(argv).await);
    }
    let sub = std::env::args()
        .nth(1)
        .expect("usage: governance <subcommand> [flags]");
    let rpc = RpcClient::new_with_commitment(rpc_url(), CommitmentConfig::confirmed());
    let settlement = settlement_id();

    match sub.as_str() {
        "init-global-config" => {
            let (global_pda, _) = zk_settlement_client::global_config_pda(&settlement);
            if rpc.get_account(&global_pda).await.is_ok() {
                println!("global_config already initialized: {global_pda}");
                return;
            }
            let authority = read_keypair_file(required_arg("--authority-keypair"))
                .expect("read authority keypair — must be the program's real upgrade authority");
            let payer =
                read_keypair_file(required_arg("--payer-keypair")).expect("read payer keypair");
            let fields = zk_settlement_client::GlobalConfigFields {
                registry_authority: pubkey_arg("--registry-authority"),
                treasury: pubkey_arg("--treasury"),
                permissionless_init_enabled: false, // forced false on-chain regardless
                reclaim_window_slots: u64_arg("--reclaim-window-slots"),
                deposit_lamports: u64_arg("--deposit-lamports"),
                default_fee_base_lamports: u64_arg("--fee-base-lamports"),
                default_fee_bps: u32_arg("--fee-bps"),
            };
            let ix = zk_settlement_client::init_global_config_ix(
                &settlement,
                &payer.pubkey(),
                &authority.pubkey(),
                fields,
            );
            let sig = send_ix(&rpc_url(), ix, &payer, &[&authority]).await;
            println!("InitGlobalConfig: global_config {global_pda}, sig {sig}");
        }
        "allow-reserved-id" => {
            let chain_id = u64_arg("--chain-id");
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read registry authority keypair");
            let payer =
                read_keypair_file(required_arg("--payer-keypair")).expect("read payer keypair");
            let (allow_pda, _) = zk_settlement_client::reserved_allow_pda(&settlement, chain_id);
            if rpc.get_account(&allow_pda).await.is_ok() {
                println!("chain {chain_id} already allowed: {allow_pda}");
                return;
            }
            let ix = zk_settlement_client::allow_reserved_id_ix(
                &settlement,
                &payer.pubkey(),
                &registry_authority.pubkey(),
                chain_id,
            );
            let sig = send_ix(&rpc_url(), ix, &payer, &[&registry_authority]).await;
            println!("AllowReservedId: chain {chain_id} allowed, marker {allow_pda}, sig {sig}");
        }
        "revoke-reserved-id" => {
            let chain_id = u64_arg("--chain-id");
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read registry authority keypair");
            let ix = zk_settlement_client::revoke_reserved_id_ix(
                &settlement,
                &registry_authority.pubkey(),
                chain_id,
            );
            let sig = send_ix(&rpc_url(), ix, &registry_authority, &[]).await;
            println!("RevokeReservedId: chain {chain_id} revoked, sig {sig}");
        }
        "set-global-config" => {
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read registry authority keypair");
            let update = zk_settlement_client::GlobalConfigUpdate {
                permissionless_init_enabled: bool_arg("--permissionless-init-enabled"),
                reclaim_window_slots: u64_arg("--reclaim-window-slots"),
                deposit_lamports: u64_arg("--deposit-lamports"),
                default_fee_base_lamports: u64_arg("--fee-base-lamports"),
                default_fee_bps: u32_arg("--fee-bps"),
            };
            let ix = zk_settlement_client::set_global_config_ix(
                &settlement,
                &registry_authority.pubkey(),
                update,
            );
            let sig = send_ix(&rpc_url(), ix, &registry_authority, &[]).await;
            println!("SetGlobalConfig applied, sig {sig}");
        }
        "propose-registry-authority" => {
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read CURRENT registry authority keypair");
            let new = pubkey_arg("--new");
            let ix = zk_settlement_client::propose_registry_authority_ix(
                &settlement,
                &registry_authority.pubkey(),
                new,
            );
            let sig = send_ix(&rpc_url(), ix, &registry_authority, &[]).await;
            println!("ProposeRegistryAuthority: proposed {new}, sig {sig}");
        }
        "accept-registry-authority" => {
            let pending = read_keypair_file(required_arg("--pending-keypair"))
                .expect("read the PROPOSED registry authority keypair");
            let ix =
                zk_settlement_client::accept_registry_authority_ix(&settlement, &pending.pubkey());
            let sig = send_ix(&rpc_url(), ix, &pending, &[]).await;
            println!(
                "AcceptRegistryAuthority: {} is now the registry authority, sig {sig}",
                pending.pubkey()
            );
        }
        "set-drift-bound" => {
            let chain_id = u64_arg("--chain-id");
            let max_drift_secs = u64_arg("--max-drift-secs");
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read registry authority keypair");
            let ix = zk_settlement_client::set_drift_bound_ix(
                &settlement,
                &registry_authority.pubkey(),
                chain_id,
                max_drift_secs,
            );
            let sig = send_ix(&rpc_url(), ix, &registry_authority, &[]).await;
            println!("SetDriftBound: chain {chain_id} max_drift_secs {max_drift_secs}, sig {sig}");
        }
        "set-registry-entry" => {
            let chain_id = u64_arg("--chain-id");
            let registry_authority = read_keypair_file(required_arg("--registry-keypair"))
                .expect("read registry authority keypair");
            let payer =
                read_keypair_file(required_arg("--payer-keypair")).expect("read payer keypair");
            let entry = zk_settlement_client::RegistryEntry {
                curve: u8_arg("--curve"),
                scheme: u8_arg("--scheme"),
                vkey_hash: hex32_arg("--vkey-hash"),
                layout_id: u8_arg("--layout-id"),
            };
            let activation_slot = match required_arg("--activation-slot").as_str() {
                "now" => rpc.get_slot().await.expect("get current slot"),
                // Retire this vkey: the tombstone activation slot `registry::find` already
                // skips for every real slot — `SetRegistryEntry` on the SAME vkey with this value is the
                // only way to retire it, immediately, no new instruction needed.
                "retire" => rome_zk_layouts::registry::RETIRED_SLOT,
                other => other.parse().unwrap_or_else(|_| {
                    panic!("--activation-slot: expected \"now\", \"retire\", or a u64, got {other}")
                }),
            };
            let ix = zk_settlement_client::set_registry_entry_ix(
                &settlement,
                &registry_authority.pubkey(),
                &payer.pubkey(),
                chain_id,
                entry,
                activation_slot,
            );
            let sig = send_ix(&rpc_url(), ix, &payer, &[&registry_authority]).await;
            if activation_slot == rome_zk_layouts::registry::RETIRED_SLOT {
                println!(
                    "SetRegistryEntry: chain {chain_id} curve {} scheme {} layout {} RETIRED, sig {sig}",
                    entry.curve, entry.scheme, entry.layout_id
                );
            } else {
                println!(
                    "SetRegistryEntry: chain {chain_id} curve {} scheme {} layout {} activation_slot {activation_slot}, sig {sig}",
                    entry.curve, entry.scheme, entry.layout_id
                );
            }
        }
        "show" => {
            print_global_config(&rpc, &settlement).await;
            if let Some(id) = arg("--chain-id") {
                print_chain_config(&rpc, &settlement, id.parse().expect("bad --chain-id")).await;
            }
        }
        other => panic!(
            "unknown subcommand: {other} (expected init-global-config, allow-reserved-id, \
             revoke-reserved-id, set-global-config, propose-registry-authority, \
             accept-registry-authority, set-drift-bound, set-registry-entry, show, \
             propose-exit-config, activate-exit-config, or show-exit-config)"
        ),
    }
}
