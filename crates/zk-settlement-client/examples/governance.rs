//! Governance CLI for a deployed zk-settlement program (registration-and-revenue proposal). Every
//! subcommand wraps one instruction builder from this crate; keys are always read from FILE PATHS and
//! never printed (only pubkeys are).
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
//! `--dry-run` (propose-exit-config / activate-exit-config only): build and sign the transaction against
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
//!     --curve 0 --scheme 1 --layout-id 1 \
//!     --vkey-hash 44916015a37f85417e8e5c5bd4c8a56498821d3380b3d90874df471a8d12ca91 \
//!     --activation-slot now \
//!     --registry-keypair /path/to/registry_authority.json --payer-keypair /path/to/payer.json [--rpc-url URL]
//!
//!   # Retire that same vkey once its dispute window has passed (or immediately on compromise) --
//!   # curve/scheme/vkey-hash/layout-id must match the entry being retired; --activation-slot switches
//!   # from a slot number to the literal word "retire".
//!   cargo run -p zk-settlement-client --features devnet-driver --example governance -- \
//!     set-registry-entry --settlement <PROGRAM_ID> --chain-id 200101 \
//!     --curve 0 --scheme 1 --layout-id 1 \
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

use base64::Engine as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    hash::Hash,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
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
fn opt_pubkey_arg(name: &str) -> Option<Pubkey> {
    arg(name).map(|s| Pubkey::from_str(&s).unwrap_or_else(|_| panic!("{name}: bad pubkey")))
}
fn opt_u64_arg(name: &str) -> Option<u64> {
    arg(name).map(|s| s.parse().unwrap_or_else(|_| panic!("{name}: bad u64")))
}
fn opt_hex20_arg(name: &str) -> Option<[u8; 20]> {
    arg(name).map(|s| {
        let bytes = hex::decode(s.trim_start_matches("0x"))
            .unwrap_or_else(|_| panic!("{name}: not valid hex"));
        bytes
            .try_into()
            .unwrap_or_else(|_| panic!("{name}: expected 20 bytes"))
    })
}
/// Bare boolean flag (no value) — `--dry-run` never takes an argument.
fn dry_run() -> bool {
    std::env::args().any(|a| a == "--dry-run")
}
fn rpc_url() -> String {
    arg("--rpc-url").unwrap_or_else(|| "https://api.devnet.solana.com".to_string())
}
fn settlement_id() -> Pubkey {
    pubkey_arg("--settlement")
}

async fn send_ix(
    rpc: &RpcClient,
    ix: solana_program::instruction::Instruction,
    payer: &solana_sdk::signature::Keypair,
    extra_signers: &[&solana_sdk::signature::Keypair],
) -> solana_sdk::signature::Signature {
    let bh = rpc.get_latest_blockhash().await.unwrap();
    let mut signers = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &signers, bh);
    rpc.send_and_confirm_transaction(&tx)
        .await
        .expect("transaction")
}

/// Signs `ix` for byte-level inspection only — an all-zero placeholder blockhash, never a real one and
/// never fetched from any RPC, so `--dry-run` needs no cluster access whatsoever. The signatures are
/// real (the given keypairs sign these exact bytes) but the transaction can never land: Solana refuses an
/// all-zero blockhash as not-yet-seen. Used only to prove the instruction's shape/discriminant before a
/// real send.
fn build_dry_run_tx(
    ix: &solana_program::instruction::Instruction,
    payer: &solana_sdk::signature::Keypair,
    extra_signers: &[&solana_sdk::signature::Keypair],
) -> Transaction {
    let mut signers = vec![payer];
    signers.extend_from_slice(extra_signers);
    Transaction::new_signed_with_payer(
        std::slice::from_ref(ix),
        Some(&payer.pubkey()),
        &signers,
        Hash::default(),
    )
}

/// Prints what `--dry-run` promises: nothing sent anywhere, the built instruction decoded back (name +
/// fields, from this crate's own `decode_instruction` — the inverse of every `*_ix` builder), its raw
/// discriminant byte, and the fully-signed transaction as the same base64 bytes a real `sendTransaction`
/// RPC call would receive.
fn print_dry_run(label: &str, ix: &solana_program::instruction::Instruction, tx: &Transaction) {
    let decoded =
        zk_settlement_client::decode_instruction(&ix.data).expect("decode built instruction");
    println!("-- dry run: {label} built and signed, nothing sent to any cluster --");
    println!("  program        {}", ix.program_id);
    println!("  discriminant   {}", ix.data[0]);
    println!("  decoded        {decoded:?}");
    // The Debug-derived print above renders `[u8; 20]`/`Pubkey` fields as raw byte arrays — readable for
    // a diff, not for a human. Print the hex/base58 forms explicitly for the one variant this CLI builds
    // with byte-array fields, so what was typed on the command line is visibly what got encoded.
    if let zk_settlement_client::SettleIx::ProposeExitConfig {
        exit_portal,
        bridge_program,
        ..
    } = &decoded
    {
        if let Some(p) = exit_portal {
            println!("  exit_portal    0x{}", hex::encode(p));
        }
        if let Some(b) = bridge_program {
            println!("  bridge_program {b}");
        }
    }
    let bytes = bincode::serialize(tx).expect("serialize tx");
    println!(
        "  tx (base64)    {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
}

/// The `pending_mask` a `ProposeExitConfig` with these four optional fields will write — the same bits
/// `programs/zk-settlement`'s `governance::propose_exit_config` computes, so `propose-exit-config` can
/// print the resulting mask without a round trip to read it back.
fn pending_mask(
    exit_portal: Option<[u8; 20]>,
    bridge_program: Option<Pubkey>,
    exit_cap: Option<u64>,
    poster_bond: Option<u64>,
) -> u8 {
    use rome_zk_layouts::exit::exit_config::{
        PENDING_MASK_BOND, PENDING_MASK_BRIDGE, PENDING_MASK_CAP, PENDING_MASK_PORTAL,
    };
    let mut mask = 0u8;
    if exit_portal.is_some() {
        mask |= PENDING_MASK_PORTAL;
    }
    if bridge_program.is_some() {
        mask |= PENDING_MASK_BRIDGE;
    }
    if exit_cap.is_some() {
        mask |= PENDING_MASK_CAP;
    }
    if poster_bond.is_some() {
        mask |= PENDING_MASK_BOND;
    }
    mask
}

/// Which `pending_mask` bits are set, by name (empty mask -> "none") — `show-exit-config`'s own
/// human-readable rendering of the bitmask `rome_zk_layouts::exit::exit_config` defines.
fn pending_mask_names(mask: u8) -> String {
    use rome_zk_layouts::exit::exit_config::{
        PENDING_MASK_BOND, PENDING_MASK_BRIDGE, PENDING_MASK_CAP, PENDING_MASK_PORTAL,
    };
    if mask == 0 {
        return "none".to_string();
    }
    let mut names = Vec::new();
    if mask & PENDING_MASK_PORTAL != 0 {
        names.push("portal");
    }
    if mask & PENDING_MASK_BRIDGE != 0 {
        names.push("bridge");
    }
    if mask & PENDING_MASK_CAP != 0 {
        names.push("cap");
    }
    if mask & PENDING_MASK_BOND != 0 {
        names.push("bond");
    }
    names.join(",")
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
            let sig = send_ix(&rpc, ix, &payer, &[&authority]).await;
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
            let sig = send_ix(&rpc, ix, &payer, &[&registry_authority]).await;
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
            let sig = send_ix(&rpc, ix, &registry_authority, &[]).await;
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
            let sig = send_ix(&rpc, ix, &registry_authority, &[]).await;
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
            let sig = send_ix(&rpc, ix, &registry_authority, &[]).await;
            println!("ProposeRegistryAuthority: proposed {new}, sig {sig}");
        }
        "accept-registry-authority" => {
            let pending = read_keypair_file(required_arg("--pending-keypair"))
                .expect("read the PROPOSED registry authority keypair");
            let ix =
                zk_settlement_client::accept_registry_authority_ix(&settlement, &pending.pubkey());
            let sig = send_ix(&rpc, ix, &pending, &[]).await;
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
            let sig = send_ix(&rpc, ix, &registry_authority, &[]).await;
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
            let sig = send_ix(&rpc, ix, &payer, &[&registry_authority]).await;
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
        "propose-exit-config" => {
            let chain_id = u64_arg("--chain-id");
            let chain_authority = read_keypair_file(required_arg("--chain-authority-keypair"))
                .expect("read chain authority keypair — must equal root.authority");
            let payer =
                read_keypair_file(required_arg("--payer-keypair")).expect("read payer keypair");
            let exit_portal = opt_hex20_arg("--exit-portal");
            let bridge_program = opt_pubkey_arg("--bridge-program");
            let exit_cap = opt_u64_arg("--exit-cap");
            let poster_bond = opt_u64_arg("--poster-bond");
            // Mirror the program's own `InvalidArgument` refusal (governance::propose_exit_config)
            // locally, before ever building a transaction: an all-None proposal writes an inert
            // exit_config (pending_mask == 0) that ActivateExitConfig can only ever refuse
            // (NoPendingExitConfig) — no reason to spend the chain authority's rent getting there.
            if exit_portal.is_none()
                && bridge_program.is_none()
                && exit_cap.is_none()
                && poster_bond.is_none()
            {
                eprintln!(
                    "refusing: propose-exit-config needs at least one of --exit-portal / \
                     --bridge-program / --exit-cap / --poster-bond"
                );
                std::process::exit(1);
            }
            let dry = dry_run();
            let activation_slot = match (arg("--activation-slot"), arg("--activation-delay-slots"))
            {
                (Some(s), None) => s
                    .parse()
                    .unwrap_or_else(|_| panic!("--activation-slot: bad u64")),
                (None, Some(d)) => {
                    let delay: u64 = d
                        .parse()
                        .unwrap_or_else(|_| panic!("--activation-delay-slots: bad u64"));
                    // No cluster to read the current slot from in --dry-run — the delay is added to an
                    // assumed current_slot of 0 and the assumption is printed, never silently applied.
                    let current_slot = if dry {
                        println!("(dry-run: current_slot assumed 0 — no live cluster to read)");
                        0
                    } else {
                        rpc.get_slot().await.expect("get current slot")
                    };
                    current_slot + delay
                }
                (Some(_), Some(_)) => {
                    panic!("pass exactly one of --activation-slot or --activation-delay-slots")
                }
                (None, None) => {
                    panic!(
                        "propose-exit-config needs --activation-slot or --activation-delay-slots"
                    )
                }
            };
            let ix = zk_settlement_client::propose_exit_config_ix(
                &settlement,
                &chain_authority.pubkey(),
                &payer.pubkey(),
                chain_id,
                exit_portal,
                bridge_program,
                exit_cap,
                poster_bond,
                activation_slot,
            );
            let mask = pending_mask(exit_portal, bridge_program, exit_cap, poster_bond);
            if dry {
                let tx = build_dry_run_tx(&ix, &payer, &[&chain_authority]);
                print_dry_run("ProposeExitConfig", &ix, &tx);
                println!(
                    "  pending_mask on activation  {mask} ({})  activation_slot {activation_slot}",
                    pending_mask_names(mask)
                );
            } else {
                let (exit_config, _) = zk_settlement_client::exit_config_pda(&settlement, chain_id);
                let sig = send_ix(&rpc, ix, &payer, &[&chain_authority]).await;
                println!(
                    "ProposeExitConfig: chain {chain_id} exit_config {exit_config} pending_mask \
                     {mask} ({}) activation_slot {activation_slot}, sig {sig}",
                    pending_mask_names(mask)
                );
            }
        }
        "activate-exit-config" => {
            let chain_id = u64_arg("--chain-id");
            let payer =
                read_keypair_file(required_arg("--payer-keypair")).expect("read payer keypair");
            let ix = zk_settlement_client::activate_exit_config_ix(&settlement, chain_id);
            if dry_run() {
                let tx = build_dry_run_tx(&ix, &payer, &[]);
                print_dry_run("ActivateExitConfig", &ix, &tx);
            } else {
                let (exit_config, _) = zk_settlement_client::exit_config_pda(&settlement, chain_id);
                match rpc.get_account(&exit_config).await {
                    Ok(acc) => {
                        let cfg = zk_settlement_client::decode_exit_config_account(&acc.data)
                            .expect("decode exit_config");
                        if cfg.pending_mask == 0 {
                            eprintln!(
                                "no pending exit config for chain {chain_id}: nothing to activate"
                            );
                            std::process::exit(1);
                        }
                        let now = rpc.get_slot().await.expect("get current slot");
                        if now < cfg.activation_slot {
                            println!(
                                "not yet — {} slots to go (current slot {now}, activation_slot {})",
                                cfg.activation_slot - now,
                                cfg.activation_slot
                            );
                            return;
                        }
                    }
                    Err(_) => {
                        eprintln!(
                            "no exit config proposed for chain {chain_id}: nothing to activate"
                        );
                        std::process::exit(1);
                    }
                }
                let sig = send_ix(&rpc, ix, &payer, &[]).await;
                println!("ActivateExitConfig: chain {chain_id}, sig {sig}");
            }
        }
        "show-exit-config" => {
            let chain_id = u64_arg("--chain-id");
            let (root_pda, _) = zk_settlement_client::root_pda(&settlement, chain_id);
            let root = match rpc.get_account(&root_pda).await {
                Ok(acc) => {
                    Some(zk_settlement_client::decode_root_account(&acc.data).expect("decode root"))
                }
                Err(_) => None,
            };
            let (exit_config_pda, _) = zk_settlement_client::exit_config_pda(&settlement, chain_id);
            let cfg = match rpc.get_account(&exit_config_pda).await {
                Ok(acc) => zk_settlement_client::decode_exit_config_account(&acc.data)
                    .expect("decode exit_config"),
                Err(_) => {
                    println!(
                        "exit_config {exit_config_pda} (chain {chain_id}): no exit config (exits disabled)"
                    );
                    return;
                }
            };
            let now = rpc.get_slot().await.expect("get current slot");
            println!("exit_config {exit_config_pda} (chain {chain_id}):");
            println!(
                "  exit_portal (current)      0x{}",
                hex::encode(cfg.exit_portal)
            );
            println!("  bridge_program (current)   {}", cfg.bridge_program);
            match &root {
                Some(r) => {
                    println!(
                        "  exit_cap_per_window (current, from root)  {}",
                        r.exit_cap_per_window
                    );
                    println!(
                        "  poster_bond (current, from root)          {}",
                        r.poster_bond
                    );
                    println!(
                        "  challenge_window_slots (from root)        {}",
                        r.challenge_window_slots
                    );
                }
                None => println!("  root {root_pda} not found — cannot read current cap/bond"),
            }
            println!(
                "  pending_mask                {} ({})",
                cfg.pending_mask,
                pending_mask_names(cfg.pending_mask)
            );
            if cfg.pending_mask & rome_zk_layouts::exit::exit_config::PENDING_MASK_PORTAL != 0 {
                println!(
                    "  pending_exit_portal         0x{}",
                    hex::encode(cfg.pending_exit_portal)
                );
            }
            if cfg.pending_mask & rome_zk_layouts::exit::exit_config::PENDING_MASK_BRIDGE != 0 {
                println!(
                    "  pending_bridge_program      {}",
                    cfg.pending_bridge_program
                );
            }
            if cfg.pending_mask & rome_zk_layouts::exit::exit_config::PENDING_MASK_CAP != 0 {
                println!("  pending_exit_cap            {}", cfg.pending_exit_cap);
            }
            if cfg.pending_mask & rome_zk_layouts::exit::exit_config::PENDING_MASK_BOND != 0 {
                println!("  pending_poster_bond         {}", cfg.pending_poster_bond);
            }
            if cfg.pending_mask == 0 {
                println!("  activation_slot             n/a (nothing pending)");
            } else if now >= cfg.activation_slot {
                println!(
                    "  activation_slot             {} — reached (current slot {now}); activatable now",
                    cfg.activation_slot
                );
            } else {
                println!(
                    "  activation_slot             {} — not yet (current slot {now}, {} slots to go)",
                    cfg.activation_slot,
                    cfg.activation_slot - now
                );
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
