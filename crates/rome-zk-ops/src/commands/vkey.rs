//! `vkey register`, `vkey show` and `vkey retire-version`: Rome's checked registration of a chain's verification key,
//! the read that prints what is registered, and the retirement of every key a ZisK release holds.
//!
//! A key belongs to one ZisK release, and the release is the `scheme` byte of its registry entry. `vkey register`
//! names it (`--zisk`, required) and refuses, before anything is read or rebuilt, a release that is not Open in the
//! release table settlement uses (`ZiskVersionUnknown`, `ZiskVersionNotOpen`).
//!
//! `vkey register` is for the registry authority. It takes what an operator sends (chain id, genesis.json, guest tag,
//! the ELF's sha256, the programVK) and sends `SetRegistryEntry` only when every one of them survives a check, in this
//! order, each refusal named:
//!
//! 0. `PortalNotCanonical` / `GenesisAllocUnexpected`: the genesis file holds the exit portal with exactly the
//!    published runtime, no storage and no balance, and no code or storage at any other address.
//! 1. `ChainIdMismatch`: the genesis's chainId, the given chain id and the chain id in the root account on the
//!    settlement program are one number. Nothing is rebuilt for a chain that is not the one registered.
//! 2. `GenesisMismatch`: while the root has not moved (no batch posted, block 0), its block hash and state root are the
//!    chain's committed genesis, and they must equal the block the node builds from the operator's genesis.json. Once
//!    the root has moved on that record is gone, so the file's sha256 is compared with the one the operator states
//!    (when given) and the one the rebuild embedded, and step 6 anchors the genesis another way.
//! 3. `ElfMismatch`: the guest is rebuilt from that genesis in the pinned guest-build image of the named release at the
//!    given tag, and its sha256 must be the operator's.
//! 4. `VkeyMismatch`: the programVK the rebuild computes (it needs the ZisK proving keys, else
//!    `ProgramVkNotComputed`) must be the operator's.
//! 5. The backed balance, while the root is still at genesis: when the genesis gives an account a balance (a whole
//!    number of lamports, else `BalanceNotWholeLamports`), the chain's exit_config must name Rome's bridge program
//!    (`BridgeNotConfigured`, `BridgeNotRome`), and the chain's vault on it must exist (`VaultMissing`), hold wrapped
//!    SOL (`VaultNotWrappedSol`), be owned by the vault authority (`VaultNotOwnedByAuthority`) and hold at least that
//!    many lamports plus every deposit still queued (`VaultUnderfunded`; the queued deposits are read off the bridge, and
//!    none can have been credited while the root is at genesis). Once the root has moved the balance may have been spent, so the anchor
//!    (step 6) ties the genesis to a key that passed this check.
//! 6. `GenesisUnanchored`: once the root has moved, nothing on chain records the genesis, so `--anchor-guest-tag` is
//!    required: the same genesis is rebuilt at that tag and must reproduce the programVK of a registered, non-retired
//!    entry. Every report prints `genesis_sha256=` so the operator's file can be recorded.
//! 7. `VkeyAlreadyActive` / `VkeyPending`: a key already registered under this release is not sent again. Sending it would move its
//!    activation slot, so an active key is never re-sent and a pending one only with `--move-pending-activation`.
//!
//! Before step 3, `VkeyUnderOtherZiskVersion`: a programVK that a non-retired entry already holds under another release is
//! refused, as the program refuses it, so the rebuild (minutes) is not started for it.
//!
//! Then it builds `SetRegistryEntry` (BN254, the release's scheme, the programVK, layout 1) and signs it with the registry authority,
//! whose key must be the one in the global config (`NotRegistryAuthority`). A dry run prints the instruction and the
//! activation slot it would set; `--confirm` sends it as a V1 transaction. Every read here is strict, in a dry run
//! too: the checks are the point.
//!
//! `vkey show` prints the release of every entry and warns about a live entry under a Closing or Withdrawn release.
//! `vkey retire-version` finds every registry account of the settlement program and retires, one `SetRegistryEntry`
//! each, every entry that is still live under the named release. A dry run lists them; `--confirm` sends.

use crate::chain::Chain;
use crate::commands::{execute, read, read_slot, Read};
use crate::error::{Mode, OpsError, Report};
use crate::genesis;
use crate::keys::{self, Signers};
use crate::rebuild::{Rebuilder, Rebuilt};
use rome_zk_layouts::registry::{
    is_zisk_scheme, zisk_release_name, CURVE_BN254, LAYOUT_ZISK_V1, MAX_ENTRIES, RETIRED_SLOT,
    SCHEME_GROTH16, ZISK_RELEASES,
};
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use veritas::Status;
use zk_settlement_client::{RegistryAccount, RegistryEntry, RegistryEntryView};

#[derive(Debug, Clone)]
pub struct RegisterVkeyRequest {
    pub settlement: Pubkey,
    pub chain_id: u64,
    pub genesis: PathBuf,
    pub guest_tag: String,
    /// The ZisK release the key is for, by name (for example `1.3.1-alpha`). The guest is rebuilt with that release's
    /// toolchain and keys, and the entry is written with that release's scheme number.
    pub zisk: String,
    /// The rome-zk-evm tag the guest build clones (the node's tag).
    pub evm_tag: String,
    /// The node tag for the anchor rebuild; the request's `evm_tag` when not given.
    pub anchor_evm_tag: Option<String>,
    /// The ZisK release the anchor key was built with; the request's `zisk` when not given. A chain moving to a new
    /// release anchors on a key of the release it is leaving.
    pub anchor_zisk: Option<String>,
    /// Rome's bridge program: the exit_config must name it, or the vault check is not made against it.
    pub bridge_program: Pubkey,
    /// The ELF's sha256 the operator sent.
    pub elf_sha256: [u8; 32],
    /// The programVK the operator sent.
    pub program_vk: [u8; 32],
    /// The genesis file's sha256 the operator states, when they sent one.
    pub genesis_sha256: Option<[u8; 32]>,
    pub registry_keypair: PathBuf,
    pub payer_keypair: PathBuf,
    /// Exactly one of these two is given.
    pub activation_slot: Option<u64>,
    pub activation_delay_slots: Option<u64>,
    /// A guest tag with a registered key for this chain; required once the root has moved past genesis.
    pub anchor_guest_tag: Option<String>,
    /// Allow a key that is registered and pending to be sent again, which moves its activation slot.
    pub move_pending_activation: bool,
}

/// Rome's bridge program on devnet, as the rollup's program list names it.
pub fn default_bridge_program() -> Pubkey {
    let list: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../deploy/rollup/programs.devnet.json"
    ))
    .expect("the program list is JSON");
    list["programs"]["zk-bridge"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("the program list names zk-bridge")
}

fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

/// Reads a chain's registry account. A missing account is `None` (a chain with no keys yet); a failed read or an
/// account that does not decode is a refusal.
async fn read_registry<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    chain_id: u64,
    report: &mut Report,
) -> Result<Option<RegistryAccount>, OpsError> {
    let (reg_pda, _) = zk_settlement_client::registry_pda(settlement, chain_id);
    match read(
        chain,
        Mode::Confirm,
        &reg_pda,
        "the verifier registry account",
        "RegistryLookupFailed",
        report,
    )
    .await?
    {
        Read::Found(d) => zk_settlement_client::decode_registry_account(&d)
            .map(Some)
            .map_err(|e| {
                OpsError::chain(
                    "RegistryUndecodable",
                    format!("the registry account {reg_pda} does not decode: {e}"),
                )
            }),
        _ => Ok(None),
    }
}

/// `active` (a slot at or before `slot`), `pending` (a later slot) or `retired`.
fn state(e: &RegistryEntryView, slot: u64) -> &'static str {
    if e.retired {
        "retired"
    } else if e.activation_slot <= slot {
        "active"
    } else {
        "pending"
    }
}

/// The scheme number a release name has in the registry's table.
fn release_scheme(name: &str, flag: &'static str) -> Result<u8, OpsError> {
    ZISK_RELEASES
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.scheme)
        .ok_or_else(|| {
            let known: Vec<&str> = ZISK_RELEASES.iter().map(|r| r.name).collect();
            OpsError::usage(
                "ZiskVersionUnknown",
                format!(
                    "{flag} {name} is not a ZisK release the registry names (known: {})",
                    known.join(", ")
                ),
            )
        })
}

/// Where a release stands, in words.
fn status_word(s: Status) -> &'static str {
    match s {
        Status::Open => "open",
        Status::Closing => "closing",
        Status::Withdrawn => "withdrawn",
    }
}

/// The scheme number of a release that takes new entries, or the refusal that says why it does not. This is the one
/// check that comes before any file, key or chain is touched.
fn open_release(name: &str) -> Result<u8, OpsError> {
    let scheme = release_scheme(name, "--zisk")?;
    let row = veritas::zisk_version(scheme).ok_or_else(|| {
        OpsError::usage(
            "ZiskVersionUnknown",
            format!("settlement's release table has no row for ZisK {name} (scheme {scheme})"),
        )
    })?;
    require_open(name, row.status)?;
    Ok(scheme)
}

/// A release takes a new registry entry only while it is Open: Closing keeps its existing keys verifying but takes no
/// new one, and Withdrawn takes nothing.
fn require_open(name: &str, status: Status) -> Result<(), OpsError> {
    if status != Status::Open {
        return Err(OpsError::usage(
            "ZiskVersionNotOpen",
            format!(
                "ZisK {name} is {}: it takes no new registry entries; register under an open release",
                status_word(status)
            ),
        ));
    }
    Ok(())
}

/// What an entry's `scheme` byte names, for a person to read.
fn release_label(scheme: u8) -> String {
    match zisk_release_name(scheme) {
        Some(n) => n.to_string(),
        None if scheme == SCHEME_GROTH16 => "groth16".to_string(),
        None => format!("unknown({scheme})"),
    }
}

/// The summary lines both `chain-status` and `vkey show` print, from one function: how many entries, whether one for
/// the proving layout is active at `slot`, and the slot the soonest pending one activates at. `./rollup check` reads
/// these from `chain-status`.
pub fn summary_lines(entries: &[RegistryEntryView], slot: u64) -> Vec<String> {
    let mut lines = vec![format!("vkey_entries={}", entries.len())];
    let proving: Vec<_> = entries
        .iter()
        .filter(|e| e.layout_id == LAYOUT_ZISK_V1 && !e.retired)
        .collect();
    let active = proving.iter().any(|e| e.activation_slot <= slot);
    lines.push(format!("vkey_active={}", if active { "yes" } else { "no" }));
    if !active {
        if let Some(next) = proving.iter().map(|e| e.activation_slot).min() {
            lines.push(format!("vkey_pending_activation_slot={next}"));
        }
    }
    lines
}

/// `vkey show`: a chain's registry entries and when each activates. Read-only and strict.
pub async fn show<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    chain_id: u64,
) -> Result<Report, OpsError> {
    let mut report = Report::default();
    let registry = read_registry(chain, settlement, chain_id, &mut report).await?;
    let slot = read_slot(chain, Mode::Confirm, &mut report)
        .await?
        .unwrap_or(0);
    let entries = registry
        .as_ref()
        .map(|r| r.entries.as_slice())
        .unwrap_or(&[]);
    report.line(format!("chain_id={chain_id}"));
    report.line(format!("slot={slot}"));
    report.lines.extend(summary_lines(entries, slot));
    for (i, e) in entries.iter().enumerate() {
        let at = if e.retired {
            "retired".to_string()
        } else {
            e.activation_slot.to_string()
        };
        let row = veritas::zisk_version(e.scheme).filter(|_| is_zisk_scheme(e.scheme));
        let standing = row.map_or("none", |v| status_word(v.status));
        report.line(format!(
            "vkey_entry_{i}=state={} layout={} curve={} scheme={} vkey={} activation_slot={at} zisk={} zisk_status={standing}",
            state(e, slot),
            e.layout_id,
            e.curve,
            e.scheme,
            hex0x(&e.vkey_hash),
            release_label(e.scheme),
        ));
        if let Some(v) = row.filter(|v| v.status != Status::Open && !e.retired) {
            report.line(format!(
                "vkey_entry_{i}_warning=ZisK {} is {}: {}",
                v.name,
                status_word(v.status),
                if v.status == Status::Withdrawn {
                    "this entry can no longer verify a proof; move the chain to an open release and retire it"
                } else {
                    "it still verifies, but a new entry cannot be added under it; move the chain to an open release before the window ends"
                }
            ));
        }
    }
    if registry.is_none() {
        report.line("no registry account: no verification key has been registered for this chain");
    }
    Ok(report)
}

/// The vault check for a genesis with a backed balance: the chain's exit_config must name Rome's bridge program, and
/// the vault on it must exist, hold wrapped SOL in a token account the vault authority owns, and hold at least
/// `lamports`.
async fn check_vault<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    rome_bridge: &Pubkey,
    chain_id: u64,
    lamports: u64,
    report: &mut Report,
) -> Result<(), OpsError> {
    let (exit_pda, _) = zk_settlement_client::exit_config_pda(settlement, chain_id);
    let not_configured = |why: String| OpsError::chain("BridgeNotConfigured", why);
    let exit = match read(
        chain,
        Mode::Confirm,
        &exit_pda,
        "the exit_config account",
        "ExitConfigLookupFailed",
        report,
    )
    .await?
    {
        Read::Found(d) => zk_settlement_client::decode_exit_config_account(&d).map_err(|e| {
            OpsError::chain(
                "ExitConfigUndecodable",
                format!("the exit_config account {exit_pda} does not decode: {e}"),
            )
        })?,
        _ => {
            return Err(not_configured(format!(
                "the genesis gives an account {lamports} lamports, but chain {chain_id} has no exit_config {exit_pda}, so there is no bridge program whose vault could hold them"
            )))
        }
    };
    if exit.bridge_program == Pubkey::default() {
        return Err(not_configured(format!(
            "the genesis gives an account {lamports} lamports, but the exit_config {exit_pda} names no bridge program"
        )));
    }
    let bridge = exit.bridge_program;
    if bridge != *rome_bridge {
        return Err(OpsError::chain(
            "BridgeNotRome",
            format!(
                "the exit_config {exit_pda} names bridge program {bridge}, not Rome's {rome_bridge}; a vault on another program proves nothing about the backed balance"
            ),
        ));
    }
    let (vault_pda, _) = zk_bridge_client::vault_config_pda(&bridge, settlement, chain_id);
    let vault = match read(
        chain,
        Mode::Confirm,
        &vault_pda,
        "the vault_config account",
        "VaultConfigFetchFailed",
        report,
    )
    .await?
    {
        Read::Found(d) => zk_bridge_client::decode_vault_config_account(&d).map_err(|e| {
            OpsError::chain(
                "VaultMissing",
                format!("the vault_config {vault_pda} on bridge {bridge} does not decode: {e}"),
            )
        })?,
        _ => {
            return Err(OpsError::chain(
                "VaultMissing",
                format!(
                    "the genesis gives an account {lamports} lamports, but bridge {bridge} has no vault_config {vault_pda} for chain {chain_id}; run `vault init` and `vault fund` first"
                ),
            ))
        }
    };
    if vault.mint != zk_bridge_client::NATIVE_MINT {
        return Err(OpsError::chain(
            "VaultNotWrappedSol",
            format!(
                "the vault holds mint {}, not wrapped SOL ({}); the backed balance is in lamports of wrapped SOL",
                vault.mint,
                zk_bridge_client::NATIVE_MINT
            ),
        ));
    }
    let (token_pda, _) =
        zk_bridge_client::vault_token_pda(&bridge, settlement, chain_id, &vault.mint);
    let token = match read(
        chain,
        Mode::Confirm,
        &token_pda,
        "the vault token account",
        "VaultTokenFetchFailed",
        report,
    )
    .await?
    {
        // An SPL token account: mint 32 bytes, owner 32 bytes, then the amount as a little-endian u64.
        Read::Found(d) if d.len() >= 72 => d,
        _ => {
            return Err(OpsError::chain(
                "VaultMissing",
                format!(
                    "the vault token account {token_pda} does not exist or is not a token account"
                ),
            ))
        }
    };
    let (authority, _) = zk_bridge_client::vault_authority_pda(&bridge, settlement, chain_id);
    let token_mint = Pubkey::new_from_array(token[0..32].try_into().expect("32 bytes"));
    let token_owner = Pubkey::new_from_array(token[32..64].try_into().expect("32 bytes"));
    if token_mint != zk_bridge_client::NATIVE_MINT {
        return Err(OpsError::chain(
            "VaultNotWrappedSol",
            format!("the vault token account {token_pda} holds mint {token_mint}, not wrapped SOL"),
        ));
    }
    if token_owner != authority {
        return Err(OpsError::chain(
            "VaultNotOwnedByAuthority",
            format!(
                "the vault token account {token_pda} is owned by {token_owner}, not the vault authority {authority}"
            ),
        ));
    }
    let held = u64::from_le_bytes(token[64..72].try_into().expect("8 bytes"));
    let pending = pending_deposits(chain, &bridge, settlement, chain_id, report).await?;
    // While the root is at genesis no deposit can have been credited or closed, so every queued deposit still sits
    // in the vault and is owed to its depositor on the rollup. Only what is left over backs the genesis balance.
    let needed = lamports.checked_add(pending).ok_or_else(|| {
        OpsError::chain(
            "VaultUnderfunded",
            format!(
                "the genesis balance {lamports} and the pending deposits {pending} overflow a u64"
            ),
        )
    })?;
    if held < needed {
        return Err(OpsError::chain(
            "VaultUnderfunded",
            format!(
                "the genesis gives an account {lamports} lamports and the queued deposits that no batch has credited yet add up to {pending}, so the vault token account {token_pda} must hold {needed}, but it holds {held}; lock the difference with `vault fund` first"
            ),
        ));
    }
    report.line(format!(
        "check VaultUnderfunded: ok (the vault holds {held} lamports of wrapped SOL; the genesis declares {lamports} and the queued deposits add {pending}, {needed} in all)"
    ));
    Ok(())
}

/// The sum, in lamports, of every deposit queued on the bridge for this chain. Called only while the root is at
/// genesis, when none of them can have been credited or closed. No queue account means no deposits, and so does an
/// empty one: anyone can send lamports to the address, but only the bridge can put data there, and `Deposit` needs
/// a queue that decodes. For wrapped SOL the record's gwei and lamports are the same unit.
async fn pending_deposits<C: Chain>(
    chain: &C,
    bridge: &Pubkey,
    settlement: &Pubkey,
    chain_id: u64,
    report: &mut Report,
) -> Result<u64, OpsError> {
    use rome_zk_layouts::deposit_queue::{deposit_queue, deposit_record};
    let (queue_pda, _) = zk_bridge_client::deposit_queue_pda(bridge, settlement, chain_id);
    let queue = match read(
        chain,
        Mode::Confirm,
        &queue_pda,
        "the deposit queue",
        "DepositQueueFetchFailed",
        report,
    )
    .await?
    {
        Read::Found(d) if d.is_empty() => return Ok(0),
        Read::Found(d) => deposit_queue::read(&d).map_err(|e| {
            OpsError::chain(
                "DepositQueueUndecodable",
                format!("the deposit queue {queue_pda} on bridge {bridge} does not decode: {e:?}"),
            )
        })?,
        _ => return Ok(0),
    };
    let mut sum: u64 = 0;
    for index in 0..queue.count {
        let (record_pda, _) =
            zk_bridge_client::deposit_record_pda(bridge, settlement, chain_id, index);
        let record = match read(
            chain,
            Mode::Confirm,
            &record_pda,
            "a deposit record",
            "DepositRecordFetchFailed",
            report,
        )
        .await?
        {
            Read::Found(d) => deposit_record::read(&d).map_err(|e| {
                OpsError::chain(
                    "DepositRecordUndecodable",
                    format!("the deposit record {index} at {record_pda} does not decode: {e:?}"),
                )
            })?,
            _ => {
                return Err(OpsError::chain(
                    "DepositRecordMissing",
                    format!(
                        "the deposit queue {queue_pda} counts {} deposits, but record {index} at {record_pda} does not exist while the root is still at genesis",
                        queue.count
                    ),
                ))
            }
        };
        sum = sum.checked_add(record.amount_gwei).ok_or_else(|| {
            OpsError::chain(
                "VaultUnderfunded",
                "the queued deposits add up to more than a u64 can hold".to_string(),
            )
        })?;
    }
    Ok(sum)
}

/// The anchor for a genesis the chain no longer records: the operator's genesis rebuilt at `tag`, with the toolchain of
/// the anchor release, must reproduce the programVK of a registered, non-retired entry of this chain under that release.
async fn check_anchor<R: Rebuilder>(
    rebuilder: &R,
    req: &RegisterVkeyRequest,
    genesis_sha256: &str,
    registry: &RegistryAccount,
    report: &mut Report,
) -> Result<(), OpsError> {
    let unanchored = |why: String| OpsError::chain("GenesisUnanchored", why);
    let Some(tag) = req.anchor_guest_tag.as_deref() else {
        return Err(unanchored(format!(
            "the chain's root has moved past genesis, so nothing on chain records the genesis it started from; give --anchor-guest-tag, a guest tag that already has a registered key for chain {}, so the genesis {genesis_sha256} can be checked against it",
            req.chain_id
        )));
    };
    let anchor_zisk = req.anchor_zisk.as_deref().unwrap_or(&req.zisk);
    let anchor_scheme = release_scheme(anchor_zisk, "--anchor-zisk")?;
    let anchor: Rebuilt = rebuilder
        .rebuild(
            &req.genesis,
            req.chain_id,
            tag,
            req.anchor_evm_tag.as_deref().unwrap_or(&req.evm_tag),
            anchor_zisk,
        )
        .await
        .map_err(|e| {
            OpsError::chain(
                "GuestBuildFailed",
                format!("the anchor rebuild at {tag}: {e}"),
            )
        })?;
    if anchor.chain_id != req.chain_id || anchor.genesis_sha256 != genesis_sha256 {
        return Err(OpsError::chain(
            "GenesisMismatch",
            format!(
                "the anchor rebuild at {tag} embeds chain {} and genesis {}, not chain {} and genesis {genesis_sha256}",
                anchor.chain_id, anchor.genesis_sha256, req.chain_id
            ),
        ));
    }
    let Some(anchor_vk) = anchor.program_vk else {
        return Err(OpsError::chain(
            "ProgramVkNotComputed",
            format!("the anchor rebuild at {tag} computed no programVK (it needs the ZisK proving keys)"),
        ));
    };
    if registry
        .entries
        .iter()
        .any(|e| !e.retired && e.scheme == anchor_scheme && e.vkey_hash == anchor_vk)
    {
        report.line(format!(
            "check GenesisUnanchored: ok (this genesis rebuilt at {tag} with ZisK {anchor_zisk} gives programVK {}, a registered key of the chain)",
            hex0x(&anchor_vk)
        ));
        Ok(())
    } else {
        Err(unanchored(format!(
            "this genesis rebuilt at {tag} with ZisK {anchor_zisk} gives programVK {}, which is not a registered, non-retired key of chain {} under that release; it may not be the genesis the chain started from (a key built with another release needs --anchor-zisk)",
            hex0x(&anchor_vk),
            req.chain_id
        )))
    }
}

/// The registry authority on chain must be the key that signs: the program refuses any other.
async fn check_registry_authority<C: Chain>(
    chain: &C,
    settlement: &Pubkey,
    registry_pk: &Pubkey,
    report: &mut Report,
) -> Result<(), OpsError> {
    let (gc_pda, _) = zk_settlement_client::global_config_pda(settlement);
    let global = match read(
        chain,
        Mode::Confirm,
        &gc_pda,
        "the global_config account",
        "GlobalConfigLookupFailed",
        report,
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
    if global.registry_authority != *registry_pk {
        return Err(OpsError::chain(
            "NotRegistryAuthority",
            format!(
                "--registry-keypair is {registry_pk}, but the registry authority recorded on chain is {}",
                global.registry_authority
            ),
        ));
    }
    Ok(())
}

/// The chain's registry account; a chain with none is refused (`RegistryMissing`).
async fn read_chain_registry<C: Chain>(
    chain: &C,
    req: &RegisterVkeyRequest,
    report: &mut Report,
) -> Result<RegistryAccount, OpsError> {
    read_registry(chain, &req.settlement, req.chain_id, report)
        .await?
        .ok_or_else(|| {
            let (reg_pda, _) = zk_settlement_client::registry_pda(&req.settlement, req.chain_id);
            OpsError::chain(
                "RegistryMissing",
                format!("chain {} has no registry account {reg_pda}", req.chain_id),
            )
        })
}

pub async fn register<C: Chain, R: Rebuilder>(
    chain: &C,
    rebuilder: &R,
    req: RegisterVkeyRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    // What the request itself says, before any key is opened or anything is read.
    let activation = match (req.activation_slot, req.activation_delay_slots) {
        (Some(_), Some(_)) => {
            return Err(OpsError::usage(
                "ActivationConflict",
                "give --activation-slot or --activation-delay-slots, not both",
            ))
        }
        (None, None) => {
            return Err(OpsError::usage(
                "ActivationMissing",
                "give --activation-slot N or --activation-delay-slots N: the slot the key becomes usable has no default",
            ))
        }
        (a, d) => (a, d),
    };
    // The release comes first: a release that takes no new entry is refused before any file is read.
    let scheme = open_release(&req.zisk)?;
    let genesis = genesis::load(&req.genesis)?;
    genesis::check_alloc(&req.genesis)?;
    if genesis.chain_id != req.chain_id {
        return Err(OpsError::usage(
            "ChainIdMismatch",
            format!(
                "{} has chainId {} but --chain-id is {}",
                req.genesis.display(),
                genesis.chain_id,
                req.chain_id
            ),
        ));
    }
    let registry_key = keys::load(&req.registry_keypair, "--registry-keypair")?;
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let registry_pk = keys::pubkey(&registry_key);
    let mut report = Report::default();
    report.line(format!("genesis_sha256={}", genesis.sha256));
    report.line(format!(
        "check ZiskVersionNotOpen: ok (ZisK {} is open; its scheme number is {scheme})",
        req.zisk
    ));

    // 1. The registered chain.
    let (root_pda, _) = zk_settlement_client::root_pda(&req.settlement, req.chain_id);
    let root = match read(
        chain,
        Mode::Confirm,
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
                    "chain {} has no root account {root_pda} under this settlement program",
                    req.chain_id
                ),
            ))
        }
    };
    if root.chain_id != req.chain_id {
        return Err(OpsError::chain(
            "ChainIdMismatch",
            format!(
                "the root account {root_pda} records chain id {}, not {}",
                root.chain_id, req.chain_id
            ),
        ));
    }
    report.line(format!(
        "check ChainIdMismatch: ok (genesis, request and root all say chain {})",
        req.chain_id
    ));

    // The registry authority must be the one the program will accept.
    check_registry_authority(chain, &req.settlement, &registry_pk, &mut report).await?;

    // 2. The genesis the chain committed to.
    if let Some(stated) = req.genesis_sha256 {
        if hex::encode(stated) != genesis.sha256 {
            return Err(OpsError::chain(
                "GenesisMismatch",
                format!(
                    "the genesis file hashes to {} but the operator stated {}",
                    genesis.sha256,
                    hex::encode(stated)
                ),
            ));
        }
    }
    let root_at_genesis =
        root.number == 0 && root.head_pending_batch == 0 && root.head_final_batch == 0;
    if root_at_genesis {
        if root.block_hash != genesis.block_hash || root.state_root != genesis.state_root {
            return Err(OpsError::chain(
                "GenesisMismatch",
                format!(
                    "the root records genesis block {} with state root {}, but this genesis.json builds block {} with state root {}",
                    hex0x(&root.block_hash),
                    hex0x(&root.state_root),
                    hex0x(&genesis.block_hash),
                    hex0x(&genesis.state_root)
                ),
            ));
        }
        report.line(
            "check GenesisMismatch: ok (the root's genesis block hash and state root equal the block this genesis.json builds)",
        );
    } else {
        report.line(format!(
            "check GenesisMismatch: file only (the root has moved past genesis at block {}, so the chain's committed genesis is no longer on chain; the genesis file's sha256 {} is compared with the one the rebuild embedded{}; the anchor check below covers the rest)",
            root.number,
            genesis.sha256,
            if req.genesis_sha256.is_some() { " and with the one the operator stated" } else { "" }
        ));
    }

    // The registry before the rebuild, for the refusal that spares it (read again after it).
    let registry = read_chain_registry(chain, &req, &mut report).await?;

    // A programVK another release already holds is refused before the rebuild, which takes minutes. The operator's value
    // is what the rebuild must reproduce, so it is the one named here.
    if let Some(e) = registry.entries.iter().find(|e| {
        !e.retired
            && e.curve == CURVE_BN254
            && is_zisk_scheme(e.scheme)
            && e.scheme != scheme
            && e.vkey_hash == req.program_vk
    }) {
        return Err(OpsError::chain(
            "VkeyUnderOtherZiskVersion",
            format!(
                "programVK {} is already registered for chain {} under ZisK {}; one programVK belongs to one release, so it is not registered under ZisK {} as well. Retire that entry first if it is the one to replace",
                hex0x(&req.program_vk),
                req.chain_id,
                release_label(e.scheme),
                req.zisk
            ),
        ));
    }

    // Every release this command will rebuild has its proving keys, before the first rebuild (which takes minutes). The
    // anchor is rebuilt only for a chain whose root has moved past genesis.
    let mut releases = vec![req.zisk.as_str()];
    if !root_at_genesis && req.anchor_guest_tag.is_some() {
        let anchor = req.anchor_zisk.as_deref().unwrap_or(&req.zisk);
        if !releases.contains(&anchor) {
            releases.push(anchor);
        }
    }
    for release in releases {
        rebuilder
            .can_rebuild(release)
            .map_err(|e| OpsError::usage("ProvingKeyDirMissing", e))?;
    }

    // 3. The rebuild.
    let built = rebuilder
        .rebuild(
            &req.genesis,
            req.chain_id,
            &req.guest_tag,
            &req.evm_tag,
            &req.zisk,
        )
        .await
        .map_err(|e| OpsError::chain("GuestBuildFailed", e))?;
    if built.chain_id != req.chain_id {
        return Err(OpsError::chain(
            "ChainIdMismatch",
            format!(
                "the rebuilt guest embeds chain id {}, not {}",
                built.chain_id, req.chain_id
            ),
        ));
    }
    if built.genesis_sha256 != genesis.sha256 {
        return Err(OpsError::chain(
            "GenesisMismatch",
            format!(
                "the rebuilt guest embeds a genesis that hashes to {}, but the genesis file hashes to {}",
                built.genesis_sha256, genesis.sha256
            ),
        ));
    }
    if built.elf_sha256 != hex::encode(req.elf_sha256) {
        return Err(OpsError::chain(
            "ElfMismatch",
            format!(
                "rebuilt at guest tag {} the ELF is {}, the operator sent {}",
                req.guest_tag,
                built.elf_sha256,
                hex::encode(req.elf_sha256)
            ),
        ));
    }
    report.line(format!(
        "check ElfMismatch: ok (rebuilt at guest tag {}: {})",
        req.guest_tag, built.elf_sha256
    ));

    // 4. The programVK.
    let rebuilt_vk = built.program_vk.ok_or_else(|| {
        OpsError::chain(
            "ProgramVkNotComputed",
            "the rebuild computed no programVK (it needs the ZisK proving keys); the operator's value is not registered unchecked",
        )
    })?;
    if rebuilt_vk != req.program_vk {
        return Err(OpsError::chain(
            "VkeyMismatch",
            format!(
                "the rebuild computes programVK {}, the operator sent {}",
                hex0x(&rebuilt_vk),
                hex0x(&req.program_vk)
            ),
        ));
    }
    report.line(format!(
        "check VkeyMismatch: ok (programVK {} under ZisK {})",
        hex0x(&rebuilt_vk),
        req.zisk
    ));

    // 5. The backed balance is in the vault, while the root is still at genesis.
    if built.genesis_balance_remainder_wei != 0 {
        return Err(OpsError::chain(
            "BalanceNotWholeLamports",
            format!(
                "the genesis balance is {} lamports and {} wei over, so it is not a whole number of lamports the vault could hold",
                built.genesis_balance, built.genesis_balance_remainder_wei
            ),
        ));
    }
    if built.genesis_balance == 0 {
        report.line("check VaultUnderfunded: not needed (the genesis gives no account a balance)");
    } else if root_at_genesis {
        check_vault(
            chain,
            &req.settlement,
            &req.bridge_program,
            req.chain_id,
            built.genesis_balance,
            &mut report,
        )
        .await?;
    } else {
        report.line(
            "check VaultUnderfunded: not made (the root has moved past genesis, so the backed balance may have been spent; the anchor below ties this genesis to a key that passed the check)",
        );
    }

    // The rebuild took minutes: the registry and the slot are read again, so the checks below and the activation slot
    // are of the chain as it is now, not as it was before the build.
    let registry = read_chain_registry(chain, &req, &mut report).await?;
    let slot = read_slot(chain, Mode::Confirm, &mut report)
        .await?
        .unwrap_or(0);

    // 6. A genesis the chain no longer records is anchored to a registered key.
    if !root_at_genesis {
        check_anchor(rebuilder, &req, &genesis.sha256, &registry, &mut report).await?;
    }

    // 7. A key already registered is not sent again.
    let existing = registry
        .entries
        .iter()
        .find(|e| e.curve == CURVE_BN254 && e.scheme == scheme && e.vkey_hash == rebuilt_vk);
    match existing {
        Some(e) if e.retired => {
            return Err(OpsError::chain(
                "VkeyRetired",
                "this key was retired for the chain; a retired key is never re-activated, only a new key can take a slot",
            ))
        }
        Some(e) if state(e, slot) == "active" => {
            return Err(OpsError::chain(
                "VkeyAlreadyActive",
                format!(
                    "this key is already registered and active since slot {} (now {slot}); sending it again would move its activation slot and take the chain's working key out of service",
                    e.activation_slot
                ),
            ))
        }
        Some(e) if !req.move_pending_activation => {
            return Err(OpsError::chain(
                "VkeyPending",
                format!(
                    "this key is already registered and activates at slot {} (now {slot}); sending it again moves that slot. Give --move-pending-activation to do that",
                    e.activation_slot
                ),
            ))
        }
        Some(e) => report.line(format!(
            "this key is already registered and pending (activation slot {}); --move-pending-activation moves it",
            e.activation_slot
        )),
        None => {
            let full = registry.entries.len() >= MAX_ENTRIES
                && !registry.entries.iter().any(|e| e.activation_slot == RETIRED_SLOT);
            if full {
                return Err(OpsError::chain(
                    "RegistryFull",
                    format!(
                        "the chain's registry holds {MAX_ENTRIES} keys and none is retired; retire one first"
                    ),
                ));
            }
        }
    }
    let activation_slot = match activation {
        (Some(a), _) => {
            if a < slot {
                return Err(OpsError::chain(
                    "ActivationInPast",
                    format!("--activation-slot {a} is before the current slot {slot}"),
                ));
            }
            a
        }
        (None, Some(d)) => slot.checked_add(d).ok_or_else(|| {
            OpsError::usage(
                "ActivationInvalid",
                "--activation-delay-slots overflows a slot number",
            )
        })?,
        (None, None) => unreachable!("checked above"),
    };

    let ix = zk_settlement_client::set_registry_entry_ix(
        &req.settlement,
        &registry_pk,
        &keys::pubkey(&payer),
        req.chain_id,
        RegistryEntry {
            curve: CURVE_BN254,
            scheme,
            vkey_hash: rebuilt_vk,
            layout_id: LAYOUT_ZISK_V1,
        },
        activation_slot,
    );
    let signers = Signers::new(payer, vec![registry_key]);
    match execute(chain, mode, "SetRegistryEntry", ix, &signers, &mut report).await? {
        Some(sig) => report.line(format!(
            "chain {} verification key registered: programVK {}, usable from slot {activation_slot} (now {slot}), sig {sig}",
            req.chain_id,
            hex0x(&rebuilt_vk)
        )),
        None => {
            report.line(format!("  activation_slot={activation_slot} (now {slot})"));
            report.line(format!(
                "  would register programVK {} for chain {}; pass --confirm to send",
                hex0x(&rebuilt_vk),
                req.chain_id
            ));
        }
    }
    Ok(report)
}

/// Where `vkey retire-version` gets every registry account of the settlement program. The chain trait reads one account
/// by address; finding them all is a different read, so it has its own seam and its own fake.
#[allow(async_fn_in_trait)]
pub trait RegistryScan {
    /// Every account of `settlement` whose data starts with the registry's magic: its address and its data.
    async fn registry_accounts(
        &self,
        settlement: &Pubkey,
    ) -> Result<Vec<(Pubkey, Vec<u8>)>, String>;
}

/// A run that has no way to list accounts (the offline run, and the tests that do not need one).
pub struct NoScan;

impl RegistryScan for NoScan {
    async fn registry_accounts(&self, _: &Pubkey) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
        Err("this run has no RPC to list the registry accounts with".to_string())
    }
}

/// The real scan: `getProgramAccounts` with a filter on the registry's magic, over the RPC the CLI was given.
pub struct RpcScan {
    pub url: String,
}

/// Base58 of a short byte string, for the RPC's memcmp filter.
fn base58(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    let mut digits: Vec<u8> = Vec::new();
    for &b in bytes {
        let mut carry = b as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut out = String::new();
    out.extend(std::iter::repeat_n('1', zeros));
    out.extend(digits.iter().rev().map(|d| ALPHABET[*d as usize] as char));
    out
}

impl RegistryScan for RpcScan {
    async fn registry_accounts(
        &self,
        settlement: &Pubkey,
    ) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
        use base64::Engine;
        let magic = rome_zk_layouts::registry::MAGIC.to_le_bytes();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getProgramAccounts",
            "params": [settlement.to_string(), {
                "encoding": "base64",
                "commitment": "confirmed",
                "filters": [{"memcmp": {"offset": 0, "bytes": base58(&magic)}}],
            }],
        });
        let resp: serde_json::Value = reqwest::Client::new()
            .post(&self.url)
            .timeout(std::time::Duration::from_secs(60))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("the RPC request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("the RPC answer is not JSON: {e}"))?;
        if let Some(e) = resp.get("error") {
            return Err(format!("the RPC refused getProgramAccounts: {e}"));
        }
        let rows = resp["result"]
            .as_array()
            .ok_or("the RPC answer has no result list")?;
        let mut out = Vec::new();
        for row in rows {
            let key: Pubkey = row["pubkey"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or("an account in the answer has no address")?;
            let data = row["account"]["data"][0]
                .as_str()
                .ok_or("an account in the answer has no data")?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("an account's data is not base64: {e}"))?;
            out.push((key, data));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct RetireVersionRequest {
    pub settlement: Pubkey,
    /// The ZisK release whose entries are retired, by name (for example `1.2.0-alpha`).
    pub zisk: String,
    pub registry_keypair: PathBuf,
    pub payer_keypair: PathBuf,
}

/// `vkey retire-version`: every entry of every chain's registry that is still live under the release `req.zisk` is
/// retired, one `SetRegistryEntry` (the entry's own key with the retired slot) each. Retiring takes effect in the slot it
/// lands in, and it is final. A dry run lists them and sends nothing. Any release the table names can be retired,
/// whatever its status: a withdrawn release's entries are the ones to clear.
pub async fn retire_version<C: Chain, S: RegistryScan>(
    chain: &C,
    scan: &S,
    req: RetireVersionRequest,
    mode: Mode,
) -> Result<Report, OpsError> {
    let scheme = release_scheme(&req.zisk, "--zisk")?;
    let registry_key = keys::load(&req.registry_keypair, "--registry-keypair")?;
    let payer = keys::load(&req.payer_keypair, "--payer-keypair")?;
    let registry_pk = keys::pubkey(&registry_key);
    let mut report = Report::default();
    check_registry_authority(chain, &req.settlement, &registry_pk, &mut report).await?;

    let mut accounts = scan.registry_accounts(&req.settlement).await.map_err(|e| {
        OpsError::chain(
            "RegistryScanFailed",
            format!(
                "the registry accounts of {} could not be listed; the client said: {e}",
                req.settlement
            ),
        )
    })?;
    accounts.sort_by_key(|(k, _)| *k);

    // Every account is decoded before anything is sent: a registry that cannot be read could hold a live entry, so it
    // stops the run and no entry is retired on a partial view.
    let mut plan: Vec<(u64, usize, RegistryEntryView)> = Vec::new();
    for (key, data) in &accounts {
        let registry = zk_settlement_client::decode_registry_account(data).map_err(|e| {
            OpsError::chain(
                "RegistryUndecodable",
                format!("the registry account {key} does not decode: {e}"),
            )
        })?;
        let (expected, _) = zk_settlement_client::registry_pda(&req.settlement, registry.chain_id);
        if expected != *key {
            return Err(OpsError::chain(
                "RegistryUndecodable",
                format!(
                    "the account {key} holds a registry for chain {}, whose registry address is {expected}",
                    registry.chain_id
                ),
            ));
        }
        for (i, e) in registry.entries.iter().enumerate() {
            if !e.retired && is_zisk_scheme(e.scheme) && e.scheme == scheme {
                plan.push((registry.chain_id, i, *e));
            }
        }
    }
    plan.sort_by_key(|(c, i, _)| (*c, *i));
    report.line(format!("registries_read={}", accounts.len()));
    report.line(format!(
        "zisk={} scheme={scheme} entries_to_retire={}",
        req.zisk,
        plan.len()
    ));
    if plan.is_empty() {
        report.line(format!(
            "no entry is live under ZisK {}: nothing to retire",
            req.zisk
        ));
        return Ok(report);
    }
    for (chain_id, i, e) in &plan {
        report.line(format!(
            "retire chain_id={chain_id} entry={i} programVK={} layout={} curve={} scheme={scheme} zisk={}",
            hex0x(&e.vkey_hash),
            e.layout_id,
            e.curve,
            req.zisk
        ));
    }
    let mut sent = 0usize;
    for (chain_id, i, e) in &plan {
        let ix = zk_settlement_client::set_registry_entry_ix(
            &req.settlement,
            &registry_pk,
            &keys::pubkey(&payer),
            *chain_id,
            RegistryEntry {
                curve: e.curve,
                scheme: e.scheme,
                vkey_hash: e.vkey_hash,
                layout_id: e.layout_id,
            },
            RETIRED_SLOT,
        );
        let signers = Signers::new(keys::copy(&payer), vec![keys::copy(&registry_key)]);
        match execute(chain, mode, "SetRegistryEntry", ix, &signers, &mut report).await {
            Ok(Some(sig)) => {
                sent += 1;
                report.line(format!("retired chain_id={chain_id} entry={i}: sig {sig}"));
            }
            Ok(None) => {}
            Err(e) => {
                return Err(OpsError::chain(
                    e.name,
                    format!(
                        "{} ({sent} of {} entries were retired before this; run the command again to retire the rest)",
                        e.detail,
                        plan.len()
                    ),
                ));
            }
        }
    }
    if mode.is_confirm() {
        report.line(format!("{sent} entries retired under ZisK {}", req.zisk));
    } else {
        report.line(format!(
            "  would retire {} entries under ZisK {}; pass --confirm to send",
            plan.len(),
            req.zisk
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;
    use crate::genesis::tests::write_genesis;
    use crate::rebuild::Rebuilt;
    use rome_zk_layouts::global_config::{self, GlobalConfigFields};
    use rome_zk_layouts::registry;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHAIN: u64 = 4_295_391_538;
    const VK: [u8; 32] = [0x5a; 32];
    const ELF: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

    struct FakeRebuilder {
        result: Result<Rebuilt, String>,
        calls: AtomicUsize,
        /// What a rebuild at a given guest tag reports instead of `result`.
        by_tag: std::sync::Mutex<std::collections::HashMap<String, Rebuilt>>,
        /// The node tag each rebuild was asked for, in order.
        evm_tags: std::sync::Mutex<Vec<String>>,
        /// The ZisK release each rebuild was asked for, in order.
        zisks: std::sync::Mutex<Vec<String>>,
        /// The releases this rebuilder holds proving keys for; `None` holds them for every release.
        keys: Option<std::collections::BTreeSet<String>>,
        /// Set when a rebuild runs, for a chain that changes while the build takes its minutes.
        rebuilt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl FakeRebuilder {
        fn new(genesis_sha256: &str) -> Self {
            Self {
                result: Ok(Rebuilt {
                    elf_sha256: ELF.to_string(),
                    genesis_sha256: genesis_sha256.to_string(),
                    chain_id: CHAIN,
                    genesis_balance: 0,
                    genesis_balance_remainder_wei: 0,
                    program_vk: Some(VK),
                }),
                calls: AtomicUsize::new(0),
                by_tag: Default::default(),
                evm_tags: Default::default(),
                zisks: Default::default(),
                keys: None,
                rebuilt: None,
            }
        }
        /// Proving keys for these releases only.
        fn keys_for(mut self, releases: &[&str]) -> Self {
            self.keys = Some(releases.iter().map(|r| r.to_string()).collect());
            self
        }
        /// A rebuild at `tag` reports the same as the default one with `f` applied.
        fn at_tag(self, tag: &str, f: impl FnOnce(&mut Rebuilt)) -> Self {
            let mut r = self.result.clone().unwrap();
            f(&mut r);
            self.by_tag.lock().unwrap().insert(tag.to_string(), r);
            self
        }
        fn with(mut self, f: impl FnOnce(&mut Rebuilt)) -> Self {
            f(self.result.as_mut().unwrap());
            self
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Rebuilder for FakeRebuilder {
        fn can_rebuild(&self, z: &str) -> Result<(), String> {
            match &self.keys {
                Some(k) if !k.contains(z) => Err(format!("no proving key directory for ZisK {z}")),
                _ => Ok(()),
            }
        }

        async fn rebuild(
            &self,
            _g: &Path,
            _c: u64,
            t: &str,
            e: &str,
            z: &str,
        ) -> Result<Rebuilt, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.evm_tags.lock().unwrap().push(e.to_string());
            self.zisks.lock().unwrap().push(z.to_string());
            if let Some(flag) = &self.rebuilt {
                flag.store(true, Ordering::SeqCst);
            }
            if let Some(r) = self.by_tag.lock().unwrap().get(t) {
                return Ok(r.clone());
            }
            self.result.clone()
        }
    }

    fn root(chain_id: u64, number: u64, g: &crate::genesis::GenesisInfo, heads: u64) -> Vec<u8> {
        rome_zk_layouts::root::write(&rome_zk_layouts::root::RootFields {
            chain_id,
            number,
            parent_hash: [0; 32],
            state_root: g.state_root,
            block_hash: g.block_hash,
            updates: 0,
            profile: 0,
            challenge_window_slots: 100,
            prove_window_slots: 100,
            proving_policy: 1,
            poster_bond: 0,
            exit_cap_per_window: 0,
            authority: [4; 32],
            head_pending_batch: heads,
            head_final_batch: heads,
            pending_count: 0,
            max_pending: 10,
        })
        .to_vec()
    }

    fn global(authority: Pubkey) -> Vec<u8> {
        let mut d = vec![0u8; global_config::LEN];
        global_config::write(
            &mut d,
            &GlobalConfigFields {
                registry_authority: authority.to_bytes(),
                treasury: [2; 32],
                permissionless_init_enabled: true,
                reclaim_window_slots: 1_000,
                deposit_lamports: 5_000_000_000,
                default_fee_base_lamports: 0,
                default_fee_bps: 0,
                pending_registry_authority: [0; 32],
            },
        );
        d
    }

    fn registry_bytes(entries: &[(u8, [u8; 32], u64)]) -> Vec<u8> {
        let rows: Vec<_> = entries
            .iter()
            .map(|(l, v, a)| (registry::SCHEME_ZISK_1_3_1, *l, *v, *a))
            .collect();
        registry_for(CHAIN, &rows)
    }

    /// A registry account of `chain_id` whose entries carry the given scheme: (scheme, layout, programVK, slot).
    fn registry_for(chain_id: u64, entries: &[(u8, u8, [u8; 32], u64)]) -> Vec<u8> {
        let mut d = vec![0u8; registry::REGISTRY_LEN_V2];
        d[registry::OFF_MAGIC..registry::OFF_MAGIC + 4]
            .copy_from_slice(&registry::MAGIC.to_le_bytes());
        d[registry::OFF_CHAIN_ID..registry::OFF_CHAIN_ID + 8]
            .copy_from_slice(&chain_id.to_le_bytes());
        d[registry::OFF_COUNT] = entries.len() as u8;
        for (i, (scheme, layout, vk, at)) in entries.iter().enumerate() {
            registry::write_entry(
                &mut d,
                i,
                &registry::RegistryEntry {
                    curve: registry::CURVE_BN254,
                    scheme: *scheme,
                    vkey_hash: *vk,
                    layout_id: *layout,
                },
                *at,
            )
            .unwrap();
        }
        d
    }

    struct Setup {
        req: RegisterVkeyRequest,
        genesis: crate::genesis::GenesisInfo,
        files: Vec<std::path::PathBuf>,
        authority: Pubkey,
    }

    fn setup(tag: &str) -> Setup {
        let gpath = write_genesis(tag, CHAIN, "0x0");
        let genesis = crate::genesis::load(&gpath).unwrap();
        let (reg, reg_path) = key_file(&format!("{tag}-r"));
        let (_, pay_path) = key_file(&format!("{tag}-p"));
        Setup {
            req: RegisterVkeyRequest {
                settlement: program(),
                chain_id: CHAIN,
                genesis: gpath.clone(),
                guest_tag: "v0.2.0".into(),
                zisk: "1.3.1-alpha".into(),
                evm_tag: "v0.2.1".into(),
                anchor_evm_tag: None,
                anchor_zisk: None,
                bridge_program: Pubkey::new_from_array(BRIDGE),
                elf_sha256: hex::decode(ELF).unwrap().try_into().unwrap(),
                program_vk: VK,
                genesis_sha256: None,
                registry_keypair: reg_path.clone(),
                payer_keypair: pay_path.clone(),
                activation_slot: Some(5_000),
                activation_delay_slots: None,
                anchor_guest_tag: None,
                move_pending_activation: false,
            },
            genesis,
            files: vec![gpath, reg_path, pay_path],
            authority: key_pubkey(&reg),
        }
    }

    fn world(s: &Setup, root_bytes: Vec<u8>, registry: Option<Vec<u8>>) -> FakeChain {
        let (rt, _) = zk_settlement_client::root_pda(&program(), CHAIN);
        let (gc, _) = zk_settlement_client::global_config_pda(&program());
        let (rg, _) = zk_settlement_client::registry_pda(&program(), CHAIN);
        let mut c = FakeChain::default()
            .with(rt, root_bytes)
            .with(gc, global(s.authority));
        if let Some(r) = registry {
            c = c.with(rg, r);
        }
        c
    }

    fn happy(s: &Setup) -> FakeChain {
        world(s, root(CHAIN, 0, &s.genesis, 0), Some(registry_bytes(&[])))
    }

    fn clean(s: Setup) {
        s.files.iter().for_each(|p| remove(p));
    }

    #[tokio::test]
    async fn a_dry_run_checks_everything_and_sends_nothing() {
        let s = setup("v1");
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let r = register(&chain, &rb, s.req.clone(), Mode::Dry)
            .await
            .unwrap();
        let t = r.lines.join("\n");
        assert_eq!(chain.sent_count(), 0);
        assert_eq!(rb.calls(), 1);
        for c in [
            "ChainIdMismatch",
            "GenesisMismatch",
            "ElfMismatch",
            "VkeyMismatch",
        ] {
            assert!(t.contains(&format!("check {c}: ok")), "{t}");
        }
        assert!(t.contains("SetRegistryEntry"), "{t}");
        assert!(
            t.contains(&format!("genesis_sha256={}", s.genesis.sha256)),
            "{t}"
        );
        assert!(t.contains("activation_slot=5000 (now 1000)"), "{t}");
        assert!(t.contains("pass --confirm to send"), "{t}");
        assert!(!r.sent());
        clean(s);
    }

    #[tokio::test]
    async fn confirm_sends_one_set_registry_entry_for_the_rebuilt_key() {
        let s = setup("v2");
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let mut req = s.req.clone();
        req.activation_slot = None;
        req.activation_delay_slots = Some(300);
        let r = register(&chain, &rb, req, Mode::Confirm).await.unwrap();
        assert_eq!(chain.tx_count(), 1);
        assert_eq!(chain.sent_count(), 1);
        let ix = chain.sent.lock().unwrap()[0].clone();
        match zk_settlement_client::decode_instruction(&ix.data).unwrap() {
            zk_settlement_client::SettleIx::SetRegistryEntry {
                chain_id,
                entry,
                activation_slot,
            } => {
                assert_eq!(chain_id, CHAIN);
                assert_eq!(entry.vkey_hash, VK);
                assert_eq!(entry.layout_id, LAYOUT_ZISK_V1);
                assert_eq!(entry.scheme, registry::SCHEME_ZISK_1_3_1);
                assert_eq!(activation_slot, 1_300); // slot 1000 + 300
            }
            other => panic!("expected SetRegistryEntry, got {other:?}"),
        }
        assert!(r.sent());
        assert!(
            r.lines.last().unwrap().contains("usable from slot 1300"),
            "{:?}",
            r.lines
        );
        clean(s);
    }

    /// Runs a refusal: nothing was sent, and the refusal has the expected name and exit code.
    async fn refused(
        s: &Setup,
        chain: &FakeChain,
        rb: &FakeRebuilder,
        req: RegisterVkeyRequest,
        name: &str,
        code: i32,
    ) -> OpsError {
        let e = register(chain, rb, req, Mode::Confirm).await.unwrap_err();
        assert_eq!(e.name, name, "{e}");
        assert_eq!(e.exit_code, code, "{e}");
        assert_eq!(chain.sent_count(), 0, "a refusal must send nothing");
        let _ = s;
        e
    }

    #[tokio::test]
    async fn chain_id_mismatch_when_the_genesis_names_another_chain() {
        let s = setup("c1");
        let mut req = s.req.clone();
        req.chain_id = CHAIN + 1;
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        refused(&s, &chain, &rb, req, "ChainIdMismatch", 2).await;
        assert_eq!(rb.calls(), 0, "nothing is rebuilt for another chain");
        clean(s);
    }

    #[tokio::test]
    async fn chain_id_mismatch_when_the_root_records_another_chain() {
        let s = setup("c2");
        let chain = world(
            &s,
            root(CHAIN + 9, 0, &s.genesis, 0),
            Some(registry_bytes(&[])),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(&s, &chain, &rb, s.req.clone(), "ChainIdMismatch", 1).await;
        assert!(e.detail.contains("records chain id"), "{e}");
        assert_eq!(rb.calls(), 0);
        clean(s);
    }

    #[tokio::test]
    async fn chain_id_mismatch_when_the_rebuilt_guest_embeds_another_id() {
        let s = setup("c3");
        let (chain, rb) = (
            happy(&s),
            FakeRebuilder::new(&s.genesis.sha256).with(|b| b.chain_id = 1),
        );
        refused(&s, &chain, &rb, s.req.clone(), "ChainIdMismatch", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn an_unregistered_chain_is_refused() {
        let s = setup("c4");
        let chain = FakeChain::default();
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, s.req.clone(), "ChainNotRegistered", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn genesis_mismatch_when_the_root_committed_another_genesis() {
        let s = setup("g1");
        let other = crate::genesis::GenesisInfo {
            block_hash: [9; 32],
            ..s.genesis.clone()
        };
        let chain = world(&s, root(CHAIN, 0, &other, 0), Some(registry_bytes(&[])));
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, s.req.clone(), "GenesisMismatch", 1).await;
        assert_eq!(rb.calls(), 0);
        clean(s);
    }

    #[tokio::test]
    async fn genesis_mismatch_when_the_state_root_differs() {
        let s = setup("g2");
        let other = crate::genesis::GenesisInfo {
            state_root: [9; 32],
            ..s.genesis.clone()
        };
        let chain = world(&s, root(CHAIN, 0, &other, 0), Some(registry_bytes(&[])));
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, s.req.clone(), "GenesisMismatch", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn genesis_mismatch_when_the_stated_hash_is_not_the_files() {
        let s = setup("g3");
        let mut req = s.req.clone();
        req.genesis_sha256 = Some([7; 32]);
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        refused(&s, &chain, &rb, req, "GenesisMismatch", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn genesis_mismatch_when_the_rebuild_embedded_another_genesis() {
        let s = setup("g4");
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&"00".repeat(32)));
        refused(&s, &chain, &rb, s.req.clone(), "GenesisMismatch", 1).await;
        clean(s);
    }

    const OLD_VK: [u8; 32] = [0x33; 32];
    const OLD_TAG: &str = "v0.1.0";

    /// A chain whose root has moved past genesis, with one registered key (`OLD_VK`, active), as after its first
    /// posted batch.
    fn moved_world(s: &Setup, registry: &[(u8, [u8; 32], u64)]) -> FakeChain {
        let moved = crate::genesis::GenesisInfo {
            block_hash: [9; 32],
            ..s.genesis.clone()
        };
        world(
            s,
            root(CHAIN, 40, &moved, 3),
            Some(registry_bytes(registry)),
        )
    }

    #[tokio::test]
    async fn once_the_root_has_moved_the_genesis_must_be_anchored_to_a_registered_key() {
        let s = setup("g5");
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        // The same genesis rebuilt at the anchor tag gives the registered key.
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        let r = register(&chain, &rb, req, Mode::Dry).await.unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("check GenesisMismatch: file only"), "{t}");
        assert!(t.contains("moved past genesis at block 40"), "{t}");
        assert!(t.contains("check GenesisUnanchored: ok"), "{t}");
        assert!(
            t.contains(&format!("genesis_sha256={}", s.genesis.sha256)),
            "{t}"
        );
        assert_eq!(
            rb.calls(),
            2,
            "the guest and the anchor are each rebuilt once"
        );
        clean(s);
    }

    #[tokio::test]
    async fn a_moved_root_without_an_anchor_tag_is_refused_unanchored() {
        let s = setup("g6");
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(&s, &chain, &rb, s.req.clone(), "GenesisUnanchored", 1).await;
        assert!(e.detail.contains("--anchor-guest-tag"), "{e}");
        clean(s);
    }

    #[tokio::test]
    async fn an_anchor_that_reproduces_no_registered_key_is_refused_unanchored() {
        let s = setup("g7");
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        // The anchor rebuild gives a key the chain never registered (a swapped genesis config would do this).
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        let rb = FakeRebuilder::new(&s.genesis.sha256)
            .at_tag(OLD_TAG, |b| b.program_vk = Some([0x44; 32]));
        refused(&s, &chain, &rb, req.clone(), "GenesisUnanchored", 1).await;
        // The anchor key is registered but retired: not an anchor.
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, RETIRED_SLOT)]);
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        refused(&s, &chain, &rb, req.clone(), "GenesisUnanchored", 1).await;
        // The anchor build computed no programVK: refused, not trusted.
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        let rb = FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = None);
        refused(&s, &chain, &rb, req.clone(), "ProgramVkNotComputed", 1).await;
        // The anchor build embedded another genesis.
        let rb = FakeRebuilder::new(&s.genesis.sha256)
            .at_tag(OLD_TAG, |b| b.genesis_sha256 = "00".repeat(32));
        refused(&s, &chain, &rb, req, "GenesisMismatch", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn elf_mismatch_when_the_sha_differs() {
        let s = setup("e1");
        let (chain, rb) = (
            happy(&s),
            FakeRebuilder::new(&s.genesis.sha256).with(|b| b.elf_sha256 = "11".repeat(32)),
        );
        let e = refused(&s, &chain, &rb, s.req.clone(), "ElfMismatch", 1).await;
        assert!(
            e.detail.contains(&"11".repeat(32)) && e.detail.contains(ELF),
            "{e}"
        );
        clean(s);
    }

    #[tokio::test]
    async fn vkey_mismatch_when_the_rebuilt_key_differs() {
        let s = setup("k1");
        let (chain, rb) = (
            happy(&s),
            FakeRebuilder::new(&s.genesis.sha256).with(|b| b.program_vk = Some([0x77; 32])),
        );
        refused(&s, &chain, &rb, s.req.clone(), "VkeyMismatch", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_rebuild_without_a_program_vk_is_refused_not_trusted() {
        let s = setup("k2");
        let (chain, rb) = (
            happy(&s),
            FakeRebuilder::new(&s.genesis.sha256).with(|b| b.program_vk = None),
        );
        refused(&s, &chain, &rb, s.req.clone(), "ProgramVkNotComputed", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_failed_rebuild_is_refused_by_name() {
        let s = setup("k3");
        let chain = happy(&s);
        let rb = FakeRebuilder {
            result: Err("docker build failed".into()),
            calls: AtomicUsize::new(0),
            by_tag: Default::default(),
            evm_tags: Default::default(),
            zisks: Default::default(),
            keys: None,
            rebuilt: None,
        };
        refused(&s, &chain, &rb, s.req.clone(), "GuestBuildFailed", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn only_the_registry_authority_can_sign() {
        let s = setup("a1");
        let (rt, _) = zk_settlement_client::root_pda(&program(), CHAIN);
        let (gc, _) = zk_settlement_client::global_config_pda(&program());
        let chain = FakeChain::default()
            .with(rt, root(CHAIN, 0, &s.genesis, 0))
            .with(gc, global(Pubkey::new_from_array([0xEE; 32])));
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, s.req.clone(), "NotRegistryAuthority", 1).await;
        assert_eq!(rb.calls(), 0);
        clean(s);
    }

    #[tokio::test]
    async fn the_activation_slot_has_no_default_and_is_not_two_things() {
        let s = setup("a2");
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let mut none = s.req.clone();
        none.activation_slot = None;
        refused(&s, &chain, &rb, none, "ActivationMissing", 2).await;
        let mut both = s.req.clone();
        both.activation_delay_slots = Some(5);
        refused(&s, &chain, &rb, both, "ActivationConflict", 2).await;
        let mut past = s.req.clone();
        past.activation_slot = Some(10); // now is 1000
        refused(&s, &chain, &rb, past, "ActivationInPast", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn registry_states_that_the_program_would_refuse_are_refused_first() {
        let s = setup("r1");
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        // no registry account at all
        let chain = world(&s, root(CHAIN, 0, &s.genesis, 0), None);
        refused(&s, &chain, &rb, s.req.clone(), "RegistryMissing", 1).await;
        // the key was retired
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_bytes(&[(LAYOUT_ZISK_V1, VK, RETIRED_SLOT)])),
        );
        refused(&s, &chain, &rb, s.req.clone(), "VkeyRetired", 1).await;
        // four live keys, none retired
        let full: Vec<_> = (1..=4u8).map(|i| (2u8, [i; 32], 0u64)).collect();
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_bytes(&full)),
        );
        refused(&s, &chain, &rb, s.req.clone(), "RegistryFull", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_key_that_is_already_active_is_refused_and_nothing_moves() {
        let s = setup("r2");
        // Active since slot 900, now is 1000. Re-sending would push it to a later slot.
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_bytes(&[(LAYOUT_ZISK_V1, VK, 900)])),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(&s, &chain, &rb, s.req.clone(), "VkeyAlreadyActive", 1).await;
        assert!(e.detail.contains("active since slot 900"), "{e}");
        // The flag for a pending key does not unlock an active one.
        let mut req = s.req.clone();
        req.move_pending_activation = true;
        refused(&s, &chain, &rb, req, "VkeyAlreadyActive", 1).await;
        // A dry run refuses too.
        let e = register(&chain, &rb, s.req.clone(), Mode::Dry)
            .await
            .unwrap_err();
        assert_eq!(e.name, "VkeyAlreadyActive");
        clean(s);
    }

    #[tokio::test]
    async fn a_pending_key_is_refused_unless_the_move_is_asked_for() {
        let s = setup("r3");
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_bytes(&[(LAYOUT_ZISK_V1, VK, 9_000)])),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(&s, &chain, &rb, s.req.clone(), "VkeyPending", 1).await;
        assert!(e.detail.contains("activates at slot 9000"), "{e}");
        assert!(e.detail.contains("--move-pending-activation"), "{e}");
        let mut req = s.req.clone();
        req.move_pending_activation = true;
        let r = register(&chain, &rb, req.clone(), Mode::Dry).await.unwrap();
        let t = r.lines.join("\n");
        assert!(
            t.contains("already registered and pending (activation slot 9000)"),
            "{t}"
        );
        assert!(t.contains("activation_slot=5000 (now 1000)"), "{t}");
        // Confirmed, the move sends one SetRegistryEntry for the same key.
        let r = register(&chain, &rb, req, Mode::Confirm).await.unwrap();
        assert!(r.sent());
        assert_eq!(chain.sent_count(), 1);
        clean(s);
    }

    // ---- the backed balance and the vault ----

    const BRIDGE: [u8; 32] = [7; 32];
    const BACKED: u64 = 1_500_000_000;

    fn exit_config(bridge: [u8; 32]) -> Vec<u8> {
        rome_zk_layouts::exit::exit_config::write(
            &rome_zk_layouts::exit::exit_config::ExitConfigFields {
                chain_id: CHAIN,
                exit_portal: [0; 20],
                bridge_program: bridge,
                pending_exit_portal: [0; 20],
                pending_bridge_program: [0; 32],
                pending_exit_cap: 0,
                pending_poster_bond: 0,
                activation_slot: 0,
                pending_mask: 0,
            },
        )
        .to_vec()
    }

    fn vault_config(mint: Pubkey) -> Vec<u8> {
        zk_bridge::state::vault_config::write(&zk_bridge::state::vault_config::VaultConfigFields {
            chain_id: CHAIN,
            settlement_program: program(),
            mint,
            mint_decimals: 9,
            authority: Pubkey::new_from_array([5u8; 32]),
        })
        .to_vec()
    }

    fn token_account(amount: u64) -> Vec<u8> {
        let (authority, _) = zk_bridge_client::vault_authority_pda(
            &Pubkey::new_from_array(BRIDGE),
            &program(),
            CHAIN,
        );
        token_account_of(zk_bridge_client::NATIVE_MINT, authority, amount)
    }

    fn token_account_of(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(&mint.to_bytes());
        d[32..64].copy_from_slice(&owner.to_bytes());
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d
    }

    /// The chain at genesis, with the exit_config naming the bridge and a vault of the given shape.
    fn backed_world(
        s: &Setup,
        exit: Option<Vec<u8>>,
        vault: Option<Vec<u8>>,
        token: Option<(Pubkey, Vec<u8>)>,
    ) -> FakeChain {
        let bridge = Pubkey::new_from_array(BRIDGE);
        let mut c = happy(s);
        if let Some(e) = exit {
            c = c.with(
                zk_settlement_client::exit_config_pda(&program(), CHAIN).0,
                e,
            );
        }
        if let Some(v) = vault {
            c = c.with(
                zk_bridge_client::vault_config_pda(&bridge, &program(), CHAIN).0,
                v,
            );
        }
        if let Some((mint, t)) = token {
            c = c.with(
                zk_bridge_client::vault_token_pda(&bridge, &program(), CHAIN, &mint).0,
                t,
            );
        }
        c
    }

    fn backed_rebuilder(s: &Setup) -> FakeRebuilder {
        FakeRebuilder::new(&s.genesis.sha256).with(|b| b.genesis_balance = BACKED)
    }

    #[tokio::test]
    async fn a_backed_balance_that_the_vault_holds_registers() {
        let s = setup("v1b");
        let mint = zk_bridge_client::NATIVE_MINT;
        for held in [BACKED, BACKED + 1] {
            let chain = backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account(held))),
            );
            let rb = backed_rebuilder(&s);
            let r = register(&chain, &rb, s.req.clone(), Mode::Confirm)
                .await
                .unwrap();
            let t = r.lines.join("\n");
            assert!(t.contains("check VaultUnderfunded: ok"), "{t}");
            assert!(
                t.contains(&format!("the vault holds {held} lamports")),
                "{t}"
            );
            assert!(r.sent());
        }
        clean(s);
    }

    #[tokio::test]
    async fn a_backed_balance_needs_a_bridge_in_the_exit_config() {
        let s = setup("v2b");
        let rb = backed_rebuilder(&s);
        // No exit_config account.
        let chain = backed_world(&s, None, None, None);
        refused(&s, &chain, &rb, s.req.clone(), "BridgeNotConfigured", 1).await;
        // An exit_config that names no bridge.
        let chain = backed_world(&s, Some(exit_config([0; 32])), None, None);
        refused(&s, &chain, &rb, s.req.clone(), "BridgeNotConfigured", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_backed_balance_with_no_vault_is_refused_vault_missing() {
        let s = setup("v3b");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        // No vault_config.
        let chain = backed_world(&s, Some(exit_config(BRIDGE)), None, None);
        refused(&s, &chain, &rb, s.req.clone(), "VaultMissing", 1).await;
        // A vault_config but no token account.
        let chain = backed_world(
            &s,
            Some(exit_config(BRIDGE)),
            Some(vault_config(mint)),
            None,
        );
        refused(&s, &chain, &rb, s.req.clone(), "VaultMissing", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_backed_balance_in_a_vault_of_another_mint_is_refused() {
        let s = setup("v4b");
        let rb = backed_rebuilder(&s);
        let mint = Pubkey::new_from_array([0x66; 32]);
        let chain = backed_world(
            &s,
            Some(exit_config(BRIDGE)),
            Some(vault_config(mint)),
            Some((mint, token_account(u64::MAX))),
        );
        refused(&s, &chain, &rb, s.req.clone(), "VaultNotWrappedSol", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn a_backed_balance_the_vault_does_not_hold_is_refused_underfunded() {
        let s = setup("v5b");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        for held in [0, BACKED - 1] {
            let chain = backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account(held))),
            );
            let e = refused(&s, &chain, &rb, s.req.clone(), "VaultUnderfunded", 1).await;
            assert!(e.detail.contains(&format!("holds {held}")), "{e}");
        }
        clean(s);
    }

    /// The chain's deposit queue and one record per amount, as the bridge holds them while no batch has been posted.
    fn with_queued_deposits(mut c: FakeChain, amounts: &[u64]) -> FakeChain {
        use rome_zk_layouts::deposit_queue::{deposit_queue as q, deposit_record as r};
        let bridge = Pubkey::new_from_array(BRIDGE);
        let mut d = vec![0u8; q::LEN];
        q::write(
            &mut d,
            &q::DepositQueueFields {
                count: amounts.len() as u64,
                head_hash: [1u8; 32],
                params: q::DepositParams::default(),
                pending: q::DepositParams::default(),
                activation_slot: 0,
            },
        );
        c = c.with(
            zk_bridge_client::deposit_queue_pda(&bridge, &program(), CHAIN).0,
            d,
        );
        for (i, amount) in amounts.iter().enumerate() {
            let mut rec = vec![0u8; r::LEN];
            r::write(
                &mut rec,
                &r::DepositRecordFields {
                    index: i as u64,
                    enqueue_unix_ts: 0,
                    sender: [9u8; 32],
                    recipient: [0xab; 20],
                    amount_gwei: *amount,
                    hash_after: [2u8; 32],
                },
            );
            c = c.with(
                zk_bridge_client::deposit_record_pda(&bridge, &program(), CHAIN, i as u64).0,
                rec,
            );
        }
        c
    }

    #[tokio::test]
    async fn a_vault_filled_only_by_a_pending_deposit_does_not_back_the_balance() {
        let s = setup("vq1");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        // The vault holds exactly the genesis balance, all of it from one deposit no batch has credited.
        let chain = with_queued_deposits(
            backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account(BACKED))),
            ),
            &[BACKED],
        );
        let e = refused(&s, &chain, &rb, s.req.clone(), "VaultUnderfunded", 1).await;
        assert!(
            e.detail.contains(&format!("add up to {BACKED}"))
                && e.detail.contains(&format!("holds {BACKED}")),
            "{e}"
        );
        clean(s);
    }

    #[tokio::test]
    async fn a_vault_holding_the_balance_and_the_pending_deposits_registers() {
        let s = setup("vq2");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        let pending = [400_000_000u64, 25, 600_000_000];
        let sum: u64 = pending.iter().sum();
        // One lamport short of the balance plus the deposits is refused; the full amount registers.
        let short = with_queued_deposits(
            backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account(BACKED + sum - 1))),
            ),
            &pending,
        );
        refused(&s, &short, &rb, s.req.clone(), "VaultUnderfunded", 1).await;
        let chain = with_queued_deposits(
            backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account(BACKED + sum))),
            ),
            &pending,
        );
        let r = register(&chain, &rb, s.req.clone(), Mode::Confirm)
            .await
            .unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("check VaultUnderfunded: ok"), "{t}");
        assert!(
            t.contains(&format!("the queued deposits add {sum}"))
                && t.contains(&format!("{} in all", BACKED + sum)),
            "{t}"
        );
        assert!(r.sent(), "{t}");
        clean(s);
    }

    #[tokio::test]
    async fn a_chain_with_no_deposit_queue_needs_only_the_genesis_balance() {
        let s = setup("vq3");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        let chain = backed_world(
            &s,
            Some(exit_config(BRIDGE)),
            Some(vault_config(mint)),
            Some((mint, token_account(BACKED))),
        );
        let r = register(&chain, &rb, s.req.clone(), Mode::Confirm)
            .await
            .unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("the queued deposits add 0"), "{t}");
        assert!(r.sent(), "{t}");
        clean(s);
    }

    #[tokio::test]
    async fn lamports_sent_to_the_queue_address_do_not_block_registration() {
        // Anyone can fund the queue's address before the queue exists, leaving an empty system account there.
        let s = setup("vq4");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        let bridge = Pubkey::new_from_array(BRIDGE);
        let chain = backed_world(
            &s,
            Some(exit_config(BRIDGE)),
            Some(vault_config(mint)),
            Some((mint, token_account(BACKED))),
        )
        .with(
            zk_bridge_client::deposit_queue_pda(&bridge, &program(), CHAIN).0,
            vec![],
        );
        let r = register(&chain, &rb, s.req.clone(), Mode::Confirm)
            .await
            .unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("the queued deposits add 0"), "{t}");
        assert!(r.sent(), "{t}");
        clean(s);
    }

    #[tokio::test]
    async fn a_genesis_without_a_balance_never_reads_the_vault() {
        let s = setup("v6b");
        // No exit_config, no vault: fine, because there is nothing to back.
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let r = register(&chain, &rb, s.req.clone(), Mode::Dry)
            .await
            .unwrap();
        assert!(
            r.lines
                .join("\n")
                .contains("check VaultUnderfunded: not needed"),
            "{:?}",
            r.lines
        );
        clean(s);
    }

    #[tokio::test]
    async fn a_balance_that_is_not_whole_lamports_is_refused() {
        let s = setup("v7b");
        let mint = zk_bridge_client::NATIVE_MINT;
        let chain = backed_world(
            &s,
            Some(exit_config(BRIDGE)),
            Some(vault_config(mint)),
            Some((mint, token_account(u64::MAX))),
        );
        let rb = backed_rebuilder(&s).with(|b| b.genesis_balance_remainder_wei = 1);
        let e = refused(&s, &chain, &rb, s.req.clone(), "BalanceNotWholeLamports", 1).await;
        assert!(e.detail.contains("1 wei over"), "{e}");
        clean(s);
    }

    #[tokio::test]
    async fn an_exit_config_naming_another_bridge_is_refused_not_rome() {
        let s = setup("v8b");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        // The chain authority chose a bridge of its own and gave it a vault that holds plenty.
        let own = Pubkey::new_from_array([9; 32]);
        let chain = backed_world(&s, Some(exit_config([9; 32])), None, None)
            .with(
                zk_bridge_client::vault_config_pda(&own, &program(), CHAIN).0,
                vault_config(mint),
            )
            .with(
                zk_bridge_client::vault_token_pda(&own, &program(), CHAIN, &mint).0,
                token_account(u64::MAX),
            );
        let e = refused(&s, &chain, &rb, s.req.clone(), "BridgeNotRome", 1).await;
        assert!(e.detail.contains(&own.to_string()), "{e}");
        clean(s);
    }

    #[tokio::test]
    async fn the_vault_token_account_must_be_wrapped_sol_owned_by_the_vault_authority() {
        let s = setup("v9b");
        let rb = backed_rebuilder(&s);
        let mint = zk_bridge_client::NATIVE_MINT;
        let bridge = Pubkey::new_from_array(BRIDGE);
        let (authority, _) = zk_bridge_client::vault_authority_pda(&bridge, &program(), CHAIN);
        let stranger = Pubkey::new_from_array([0x44; 32]);
        for (tag_mint, owner, name) in [
            (stranger, authority, "VaultNotWrappedSol"),
            (mint, stranger, "VaultNotOwnedByAuthority"),
        ] {
            let chain = backed_world(
                &s,
                Some(exit_config(BRIDGE)),
                Some(vault_config(mint)),
                Some((mint, token_account_of(tag_mint, owner, u64::MAX))),
            );
            refused(&s, &chain, &rb, s.req.clone(), name, 1).await;
        }
        clean(s);
    }

    #[tokio::test]
    async fn the_vault_is_not_read_once_the_root_has_moved() {
        let s = setup("v10b");
        // The backed account has since spent part of its balance: the vault holds less than the genesis gave it, and
        // nothing about a vault is on this chain at all. The anchored key still registers.
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        let rb = backed_rebuilder(&s).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        let r = register(&chain, &rb, req, Mode::Confirm).await.unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("check VaultUnderfunded: not made"), "{t}");
        assert!(t.contains("check GenesisUnanchored: ok"), "{t}");
        assert!(r.sent());
        clean(s);
    }

    #[tokio::test]
    async fn the_anchor_rebuild_uses_the_anchor_node_tag_or_the_requests() {
        let s = setup("v11b");
        let chain = moved_world(&s, &[(LAYOUT_ZISK_V1, OLD_VK, 100)]);
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        // Not given: the anchor builds on the request's node tag.
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        register(&chain, &rb, req.clone(), Mode::Dry).await.unwrap();
        assert_eq!(*rb.evm_tags.lock().unwrap(), vec!["v0.2.1", "v0.2.1"]);
        // Given: the anchor builds on its own, the registered key's build is unchanged.
        req.anchor_evm_tag = Some("v0.1.9".into());
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        register(&chain, &rb, req, Mode::Dry).await.unwrap();
        assert_eq!(*rb.evm_tags.lock().unwrap(), vec!["v0.2.1", "v0.1.9"]);
        clean(s);
    }

    #[tokio::test]
    async fn a_genesis_with_another_portal_or_other_code_is_refused_before_anything_is_read() {
        let s = setup("v12b");
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let chain = happy(&s);
        let original = std::fs::read_to_string(&s.req.genesis).unwrap();
        // Another runtime at the portal address.
        let swapped = original.replacen(
            crate::genesis::EXIT_PORTAL_RUNTIME_HEX
                .trim()
                .trim_start_matches("0x"),
            "6000",
            1,
        );
        assert_ne!(swapped, original);
        std::fs::write(&s.req.genesis, &swapped).unwrap();
        refused(&s, &chain, &rb, s.req.clone(), "PortalNotCanonical", 2).await;
        // Code at the funded account's address.
        let with_code = original.replacen(
            r#""0x1111111111111111111111111111111111111111":{"balance""#,
            r#""0x1111111111111111111111111111111111111111":{"code":"0x6000","balance""#,
            1,
        );
        assert_ne!(with_code, original);
        std::fs::write(&s.req.genesis, &with_code).unwrap();
        refused(&s, &chain, &rb, s.req.clone(), "GenesisAllocUnexpected", 2).await;
        assert_eq!(rb.calls(), 0);
        clean(s);
    }

    #[tokio::test]
    async fn an_unreadable_key_file_is_refused_without_reading_the_chain() {
        let s = setup("f1");
        let mut req = s.req.clone();
        req.registry_keypair = std::path::PathBuf::from("/nonexistent/key.json");
        let chain = FakeChain::default();
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, req, "KeypairUnreadable", 2).await;
        clean(s);
    }

    #[tokio::test]
    async fn show_prints_every_entry_with_its_state_and_activation_slot() {
        let reg = registry_bytes(&[
            (LAYOUT_ZISK_V1, VK, 900),
            (LAYOUT_ZISK_V1, [8; 32], 5_000),
            (2, [9; 32], RETIRED_SLOT),
        ]);
        let (rg, _) = zk_settlement_client::registry_pda(&program(), CHAIN);
        let chain = FakeChain::default().with(rg, reg);
        let r = show(&chain, &program(), CHAIN).await.unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("vkey_entries=3"), "{t}");
        assert!(t.contains("vkey_active=yes"), "{t}");
        assert!(
            t.contains(&format!(
                "vkey_entry_0=state=active layout=1 curve=0 scheme=2 vkey={} activation_slot=900",
                hex0x(&VK)
            )),
            "{t}"
        );
        assert!(
            t.contains("vkey_entry_1=state=pending") && t.contains("activation_slot=5000"),
            "{t}"
        );
        assert!(
            t.contains("vkey_entry_2=state=retired") && t.contains("activation_slot=retired"),
            "{t}"
        );
        assert_eq!(chain.sent_count(), 0);
    }

    #[tokio::test]
    async fn show_for_a_chain_without_a_registry_says_so_and_a_failed_read_is_refused() {
        let chain = FakeChain::default();
        let t = show(&chain, &program(), CHAIN)
            .await
            .unwrap()
            .lines
            .join("\n");
        assert!(
            t.contains("vkey_entries=0") && t.contains("no registry account"),
            "{t}"
        );
        let (rg, _) = zk_settlement_client::registry_pda(&program(), CHAIN);
        let chain = FakeChain::default().failing(rg, "429");
        assert_eq!(
            show(&chain, &program(), CHAIN).await.unwrap_err().name,
            "RegistryLookupFailed"
        );
    }

    #[test]
    fn the_summary_lines_are_the_ones_chain_status_prints() {
        // One function builds them for both commands; a pending key says when it activates.
        let e = |at| RegistryEntryView {
            curve: 0,
            scheme: 1,
            vkey_hash: [1; 32],
            layout_id: LAYOUT_ZISK_V1,
            activation_slot: at,
            retired: false,
        };
        assert_eq!(
            summary_lines(&[e(5_000)], 1_000),
            vec![
                "vkey_entries=1",
                "vkey_active=no",
                "vkey_pending_activation_slot=5000"
            ]
        );
        assert_eq!(
            summary_lines(&[e(900)], 1_000),
            vec!["vkey_entries=1", "vkey_active=yes"]
        );
    }

    // ---- the release -------------------------------------------------------------------------------------------------

    const V120: u8 = registry::SCHEME_ZISK_1_2_0;
    const V131: u8 = registry::SCHEME_ZISK_1_3_1;

    #[tokio::test]
    async fn a_release_that_is_not_open_is_refused_before_a_key_file_or_the_chain_is_touched() {
        let s = setup("z1");
        let mut req = s.req.clone();
        req.zisk = "1.2.0-alpha".into();
        // Not even the key file is read: the release is the first thing checked.
        req.registry_keypair = std::path::PathBuf::from("/nonexistent/key.json");
        let chain = FakeChain::default();
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(&s, &chain, &rb, req, "ZiskVersionNotOpen", 2).await;
        assert!(e.detail.contains("1.2.0-alpha is withdrawn"), "{e}");
        assert_eq!(rb.calls(), 0, "a refused release must not start a rebuild");
        clean(s);
    }

    #[tokio::test]
    async fn a_release_the_registry_does_not_name_is_refused() {
        let s = setup("z2");
        let mut req = s.req.clone();
        req.zisk = "9.9.9-alpha".into();
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let e = refused(&s, &chain, &rb, req, "ZiskVersionUnknown", 2).await;
        assert!(
            e.detail.contains("1.2.0-alpha") && e.detail.contains("1.3.1-alpha"),
            "the refusal lists the releases the registry names: {e}"
        );
        assert_eq!(rb.calls(), 0);
        clean(s);
    }

    #[test]
    fn a_status_is_named_in_words() {
        assert_eq!(status_word(Status::Open), "open");
        assert_eq!(status_word(Status::Closing), "closing");
        assert_eq!(status_word(Status::Withdrawn), "withdrawn");
    }

    #[tokio::test]
    async fn the_rebuild_is_asked_for_the_requests_release() {
        let s = setup("z3");
        let (chain, rb) = (happy(&s), FakeRebuilder::new(&s.genesis.sha256));
        let r = register(&chain, &rb, s.req.clone(), Mode::Dry)
            .await
            .unwrap();
        assert_eq!(*rb.zisks.lock().unwrap(), vec!["1.3.1-alpha"]);
        let t = r.lines.join("\n");
        assert!(t.contains("check ZiskVersionNotOpen: ok"), "{t}");
        assert!(t.contains("under ZisK 1.3.1-alpha"), "{t}");
        clean(s);
    }

    #[tokio::test]
    async fn a_programvk_another_release_holds_is_refused_before_the_rebuild() {
        let s = setup("z4");
        // The key the operator sent is live under 1.2.0.
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_for(CHAIN, &[(V120, LAYOUT_ZISK_V1, VK, 100)])),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = refused(
            &s,
            &chain,
            &rb,
            s.req.clone(),
            "VkeyUnderOtherZiskVersion",
            1,
        )
        .await;
        assert!(e.detail.contains("1.2.0-alpha"), "{e}");
        assert_eq!(
            rb.calls(),
            0,
            "refused before the rebuild, which takes minutes"
        );
        clean(s);
    }

    #[tokio::test]
    async fn a_retired_entry_under_another_release_or_another_key_does_not_block_a_registration() {
        let s = setup("z5");
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_for(
                CHAIN,
                &[
                    (V120, LAYOUT_ZISK_V1, VK, RETIRED_SLOT),
                    (V120, LAYOUT_ZISK_V1, [7; 32], 100),
                ],
            )),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let r = register(&chain, &rb, s.req.clone(), Mode::Confirm)
            .await
            .unwrap();
        assert!(r.sent(), "{:?}", r.lines);
        let ix = chain.sent.lock().unwrap()[0].clone();
        match zk_settlement_client::decode_instruction(&ix.data).unwrap() {
            zk_settlement_client::SettleIx::SetRegistryEntry { entry, .. } => {
                assert_eq!(entry.scheme, V131, "the entry carries the release's scheme");
            }
            other => panic!("expected SetRegistryEntry, got {other:?}"),
        }
        clean(s);
    }

    #[tokio::test]
    async fn the_same_key_under_this_release_is_still_already_active() {
        let s = setup("z6");
        let chain = world(
            &s,
            root(CHAIN, 0, &s.genesis, 0),
            Some(registry_for(CHAIN, &[(V131, LAYOUT_ZISK_V1, VK, 100)])),
        );
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        refused(&s, &chain, &rb, s.req.clone(), "VkeyAlreadyActive", 1).await;
        clean(s);
    }

    #[tokio::test]
    async fn the_anchor_is_rebuilt_in_its_own_release_and_matches_only_a_key_of_that_release() {
        let s = setup("z7");
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        // The chain's registered key is a 1.2.0 key; the new key is for 1.3.1.
        let chain = world(
            &s,
            root(CHAIN, 40, &s.genesis, 3),
            Some(registry_for(CHAIN, &[(V120, LAYOUT_ZISK_V1, OLD_VK, 100)])),
        );
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        // Without --anchor-zisk the anchor is rebuilt under 1.3.1, and no 1.3.1 entry holds that key.
        let e = register(&chain, &rb, req.clone(), Mode::Dry)
            .await
            .unwrap_err();
        assert_eq!(e.name, "GenesisUnanchored", "{e}");
        assert!(e.detail.contains("--anchor-zisk"), "{e}");
        // With it, the anchor is rebuilt under 1.2.0 and matches the 1.2.0 entry.
        req.anchor_zisk = Some("1.2.0-alpha".into());
        let rb =
            FakeRebuilder::new(&s.genesis.sha256).at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        let r = register(&chain, &rb, req.clone(), Mode::Dry).await.unwrap();
        assert!(r.lines.join("\n").contains("check GenesisUnanchored: ok"));
        assert_eq!(
            *rb.zisks.lock().unwrap(),
            vec!["1.3.1-alpha", "1.2.0-alpha"]
        );
        // A release the registry does not name is refused for the anchor too.
        req.anchor_zisk = Some("0.0.1".into());
        let rb = FakeRebuilder::new(&s.genesis.sha256);
        let e = register(&chain, &rb, req, Mode::Dry).await.unwrap_err();
        assert_eq!(e.name, "ZiskVersionUnknown", "{e}");
        clean(s);
    }

    #[test]
    fn a_closing_release_takes_no_new_key_and_an_open_one_does() {
        let e = require_open("9.0.0-alpha", Status::Closing).unwrap_err();
        assert_eq!(e.name, "ZiskVersionNotOpen", "{e}");
        assert_eq!(e.exit_code, 2, "{e}");
        assert!(e.detail.contains("9.0.0-alpha is closing"), "{e}");
        assert_eq!(
            require_open("9.0.0-alpha", Status::Withdrawn)
                .unwrap_err()
                .name,
            "ZiskVersionNotOpen"
        );
        assert!(require_open("1.3.1-alpha", Status::Open).is_ok());
    }

    // ---- the proving keys of each release ----------------------------------------------------------------------------

    /// A chain whose root has moved and whose registry holds one 1.2.0 key, so an anchor in 1.2.0 can match.
    fn moved_world_with_a_120_key(s: &Setup) -> FakeChain {
        world(
            s,
            root(CHAIN, 40, &s.genesis, 3),
            Some(registry_for(CHAIN, &[(V120, LAYOUT_ZISK_V1, OLD_VK, 100)])),
        )
    }

    #[tokio::test]
    async fn an_anchor_release_with_no_proving_keys_is_refused_by_name_before_any_rebuild() {
        let s = setup("p1");
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        req.anchor_zisk = Some("1.2.0-alpha".into());
        let chain = moved_world_with_a_120_key(&s);
        // Keys for the new release only: the anchor's release has none.
        let rb = FakeRebuilder::new(&s.genesis.sha256).keys_for(&["1.3.1-alpha"]);
        let e = register(&chain, &rb, req.clone(), Mode::Dry)
            .await
            .unwrap_err();
        assert_eq!(e.name, "ProvingKeyDirMissing", "{e}");
        assert!(e.detail.contains("1.2.0-alpha"), "names the release: {e}");
        assert_eq!(rb.calls(), 0, "refused before the first rebuild");
        // The new release's own keys missing is the same refusal, naming that release.
        let rb = FakeRebuilder::new(&s.genesis.sha256).keys_for(&["1.2.0-alpha"]);
        let e = register(&chain, &rb, req.clone(), Mode::Dry)
            .await
            .unwrap_err();
        assert_eq!(e.name, "ProvingKeyDirMissing", "{e}");
        assert!(e.detail.contains("1.3.1-alpha"), "{e}");
        assert_eq!(rb.calls(), 0);
        // Both releases' keys: each rebuild is asked for its own release, in order.
        let rb = FakeRebuilder::new(&s.genesis.sha256)
            .keys_for(&["1.2.0-alpha", "1.3.1-alpha"])
            .at_tag(OLD_TAG, |b| b.program_vk = Some(OLD_VK));
        register(&chain, &rb, req, Mode::Dry).await.unwrap();
        assert_eq!(
            *rb.zisks.lock().unwrap(),
            vec!["1.3.1-alpha", "1.2.0-alpha"]
        );
        clean(s);
    }

    #[tokio::test]
    async fn a_chain_at_genesis_needs_no_keys_for_an_anchor_it_does_not_rebuild() {
        let s = setup("p2");
        let mut req = s.req.clone();
        req.anchor_guest_tag = Some(OLD_TAG.into());
        req.anchor_zisk = Some("1.2.0-alpha".into());
        let (chain, rb) = (
            happy(&s),
            FakeRebuilder::new(&s.genesis.sha256).keys_for(&["1.3.1-alpha"]),
        );
        register(&chain, &rb, req, Mode::Dry).await.unwrap();
        assert_eq!(*rb.zisks.lock().unwrap(), vec!["1.3.1-alpha"]);
        clean(s);
    }

    // ---- the chain after the rebuild ---------------------------------------------------------------------------------

    /// A chain that answers from `before` until a rebuild has run, then from `after`: the registry and the slot move
    /// while the build takes its minutes.
    struct MovesDuringRebuild {
        before: FakeChain,
        after: FakeChain,
        rebuilt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MovesDuringRebuild {
        fn now(&self) -> &FakeChain {
            if self.rebuilt.load(Ordering::SeqCst) {
                &self.after
            } else {
                &self.before
            }
        }
    }

    impl Chain for MovesDuringRebuild {
        async fn account(&self, key: &Pubkey) -> Result<Option<Vec<u8>>, String> {
            self.now().account(key).await
        }
        async fn slot(&self) -> Result<u64, String> {
            self.now().slot().await
        }
        async fn genesis(&self, evm_rpc: &str) -> Result<crate::chain::Genesis, String> {
            self.now().genesis(evm_rpc).await
        }
        async fn send(
            &self,
            ixs: &[solana_program::instruction::Instruction],
            signers: &Signers,
        ) -> Result<String, String> {
            self.now().send(ixs, signers).await
        }
    }

    fn moving(
        s: &Setup,
        registry_after: Vec<u8>,
        slot_after: u64,
    ) -> (MovesDuringRebuild, FakeRebuilder) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut after = world(s, root(CHAIN, 0, &s.genesis, 0), Some(registry_after));
        after.slot = Ok(slot_after);
        let mut rb = FakeRebuilder::new(&s.genesis.sha256);
        rb.rebuilt = Some(flag.clone());
        (
            MovesDuringRebuild {
                before: happy(s),
                after,
                rebuilt: flag,
            },
            rb,
        )
    }

    #[tokio::test]
    async fn the_slot_is_read_again_after_the_rebuild() {
        let s = setup("m1");
        let mut req = s.req.clone();
        req.activation_slot = None;
        req.activation_delay_slots = Some(30);
        let (chain, rb) = moving(&s, registry_bytes(&[]), 9_000);
        let r = register(&chain, &rb, req, Mode::Dry).await.unwrap();
        let t = r.lines.join("\n");
        assert!(t.contains("activation_slot=9030 (now 9000)"), "{t}");
        clean(s);
    }

    #[tokio::test]
    async fn the_registry_is_read_again_after_the_rebuild() {
        let s = setup("m2");
        // While the guest was building, this very key was registered and has become active.
        let (chain, rb) = moving(
            &s,
            registry_for(CHAIN, &[(V131, LAYOUT_ZISK_V1, VK, 100)]),
            9_000,
        );
        let e = register(&chain, &rb, s.req.clone(), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(e.name, "VkeyAlreadyActive", "{e}");
        assert_eq!(chain.after.sent_count(), 0);
        clean(s);
    }

    #[tokio::test]
    async fn show_prints_the_release_of_every_entry_and_warns_about_a_live_one_under_a_withdrawn_release(
    ) {
        let reg = registry_for(
            CHAIN,
            &[
                (V131, LAYOUT_ZISK_V1, VK, 900),
                (V120, LAYOUT_ZISK_V1, [8; 32], 900),
                (V120, LAYOUT_ZISK_V1, [9; 32], RETIRED_SLOT),
            ],
        );
        let (rg, _) = zk_settlement_client::registry_pda(&program(), CHAIN);
        let chain = FakeChain::default().with(rg, reg);
        let t = show(&chain, &program(), CHAIN)
            .await
            .unwrap()
            .lines
            .join("\n");
        assert!(
            t.contains("scheme=2") && t.contains("zisk=1.3.1-alpha zisk_status=open"),
            "{t}"
        );
        assert!(t.contains("zisk=1.2.0-alpha zisk_status=withdrawn"), "{t}");
        assert!(
            t.contains("vkey_entry_1_warning=ZisK 1.2.0-alpha is withdrawn"),
            "{t}"
        );
        assert!(
            !t.contains("vkey_entry_0_warning"),
            "an open release has no warning: {t}"
        );
        assert!(
            !t.contains("vkey_entry_2_warning"),
            "a retired entry has no warning: {t}"
        );
    }

    #[tokio::test]
    async fn show_names_an_entry_whose_scheme_is_no_release() {
        let reg = registry_for(CHAIN, &[(77, LAYOUT_ZISK_V1, VK, 900)]);
        let (rg, _) = zk_settlement_client::registry_pda(&program(), CHAIN);
        let chain = FakeChain::default().with(rg, reg);
        let t = show(&chain, &program(), CHAIN)
            .await
            .unwrap()
            .lines
            .join("\n");
        assert!(t.contains("zisk=unknown(77) zisk_status=none"), "{t}");
    }

    // ---- retire-version ----------------------------------------------------------------------------------------------

    struct FakeScan(Result<Vec<(Pubkey, Vec<u8>)>, String>);

    impl RegistryScan for FakeScan {
        async fn registry_accounts(&self, _: &Pubkey) -> Result<Vec<(Pubkey, Vec<u8>)>, String> {
            self.0.clone()
        }
    }

    const OTHER_CHAIN: u64 = 77;

    fn registry_account(chain_id: u64, rows: &[(u8, u8, [u8; 32], u64)]) -> (Pubkey, Vec<u8>) {
        let (pda, _) = zk_settlement_client::registry_pda(&program(), chain_id);
        (pda, registry_for(chain_id, rows))
    }

    /// Two chains: chain A has two live 1.2.0 entries, a retired 1.2.0 one and a live 1.3.1 one; chain B has one live
    /// 1.2.0 entry.
    fn two_chains() -> Vec<(Pubkey, Vec<u8>)> {
        vec![
            registry_account(
                CHAIN,
                &[
                    (V120, LAYOUT_ZISK_V1, [1; 32], 100),
                    (V131, LAYOUT_ZISK_V1, [2; 32], 100),
                    (V120, LAYOUT_ZISK_V1, [3; 32], RETIRED_SLOT),
                    (V120, 2, [4; 32], 5_000),
                ],
            ),
            registry_account(OTHER_CHAIN, &[(V120, LAYOUT_ZISK_V1, [5; 32], 100)]),
        ]
    }

    fn retire_setup(tag: &str) -> (RetireVersionRequest, Vec<std::path::PathBuf>, FakeChain) {
        let (reg, reg_path) = key_file(&format!("{tag}-r"));
        let (_, pay_path) = key_file(&format!("{tag}-p"));
        let (gc, _) = zk_settlement_client::global_config_pda(&program());
        let chain = FakeChain::default().with(gc, global(key_pubkey(&reg)));
        (
            RetireVersionRequest {
                settlement: program(),
                zisk: "1.2.0-alpha".into(),
                registry_keypair: reg_path.clone(),
                payer_keypair: pay_path.clone(),
            },
            vec![reg_path, pay_path],
            chain,
        )
    }

    #[tokio::test]
    async fn a_retire_dry_run_lists_one_retirement_per_live_entry_and_sends_nothing() {
        let (req, files, chain) = retire_setup("t1");
        let scan = FakeScan(Ok(two_chains()));
        let r = retire_version(&chain, &scan, req, Mode::Dry).await.unwrap();
        let t = r.lines.join("\n");
        assert_eq!(chain.sent_count(), 0, "{t}");
        assert!(t.contains("registries_read=2"), "{t}");
        assert!(
            t.contains("zisk=1.2.0-alpha scheme=1 entries_to_retire=3"),
            "{t}"
        );
        let listed: Vec<&str> = t
            .lines()
            .filter(|l| l.starts_with("retire chain_id="))
            .collect();
        assert_eq!(listed.len(), 3, "{t}");
        assert!(
            listed[0].starts_with(&format!("retire chain_id={OTHER_CHAIN} entry=0 ")),
            "{t}"
        );
        assert!(
            listed[1].contains(&format!("chain_id={CHAIN} entry=0 ")),
            "{t}"
        );
        assert!(
            listed[2].contains(&format!("chain_id={CHAIN} entry=3 ")),
            "{t}"
        );
        assert!(
            !t.contains(&hex0x(&[2; 32])),
            "the 1.3.1 entry is not listed: {t}"
        );
        assert!(
            !t.contains(&hex0x(&[3; 32])),
            "a retired entry is not listed: {t}"
        );
        assert!(t.contains("pass --confirm to send"), "{t}");
        assert!(!r.sent());
        files.iter().for_each(|p| remove(p));
    }

    #[tokio::test]
    async fn a_confirmed_retirement_sends_one_retired_slot_instruction_per_entry() {
        let (req, files, chain) = retire_setup("t2");
        let scan = FakeScan(Ok(two_chains()));
        let r = retire_version(&chain, &scan, req, Mode::Confirm)
            .await
            .unwrap();
        assert!(r.sent());
        assert_eq!(chain.sent_count(), 3);
        assert_eq!(chain.tx_count(), 3, "one transaction per entry");
        let mut seen = Vec::new();
        for ix in chain.sent.lock().unwrap().iter() {
            match zk_settlement_client::decode_instruction(&ix.data).unwrap() {
                zk_settlement_client::SettleIx::SetRegistryEntry {
                    chain_id,
                    entry,
                    activation_slot,
                } => {
                    assert_eq!(activation_slot, RETIRED_SLOT);
                    assert_eq!(entry.scheme, V120);
                    seen.push((chain_id, entry.vkey_hash[0], entry.layout_id));
                }
                other => panic!("expected SetRegistryEntry, got {other:?}"),
            }
        }
        assert_eq!(
            seen,
            vec![
                (OTHER_CHAIN, 5, LAYOUT_ZISK_V1),
                (CHAIN, 1, LAYOUT_ZISK_V1),
                (CHAIN, 4, 2)
            ]
        );
        assert!(r
            .lines
            .join("\n")
            .contains("3 entries retired under ZisK 1.2.0-alpha"));
        files.iter().for_each(|p| remove(p));
    }

    #[tokio::test]
    async fn retiring_a_release_nothing_holds_sends_nothing() {
        let (mut req, files, chain) = retire_setup("t3");
        req.zisk = "1.3.1-alpha".into();
        let scan = FakeScan(Ok(vec![registry_account(
            CHAIN,
            &[(V120, LAYOUT_ZISK_V1, [1; 32], 100)],
        )]));
        let r = retire_version(&chain, &scan, req, Mode::Confirm)
            .await
            .unwrap();
        assert!(!r.sent());
        assert_eq!(chain.sent_count(), 0);
        assert!(
            r.lines.join("\n").contains("nothing to retire"),
            "{:?}",
            r.lines
        );
        files.iter().for_each(|p| remove(p));
    }

    #[tokio::test]
    async fn retire_version_refuses_what_it_cannot_be_sure_of() {
        // An unknown release.
        let (mut req, files, chain) = retire_setup("t4");
        req.zisk = "0.0.1".into();
        let e = retire_version(&chain, &FakeScan(Ok(vec![])), req.clone(), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(e.name, "ZiskVersionUnknown", "{e}");
        // A scan that failed is not an empty registry.
        req.zisk = "1.2.0-alpha".into();
        let e = retire_version(
            &chain,
            &FakeScan(Err("429".into())),
            req.clone(),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(e.name, "RegistryScanFailed", "{e}");
        // One account that does not decode stops the run before anything is sent, even if another is fine.
        let mut accounts = two_chains();
        accounts.push((Pubkey::new_from_array([6; 32]), vec![0u8; 4]));
        let e = retire_version(&chain, &FakeScan(Ok(accounts)), req.clone(), Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(e.name, "RegistryUndecodable", "{e}");
        // An account at an address that is not its chain's registry address.
        let (_, data) = registry_account(CHAIN, &[(V120, LAYOUT_ZISK_V1, [1; 32], 100)]);
        let e = retire_version(
            &chain,
            &FakeScan(Ok(vec![(Pubkey::new_from_array([6; 32]), data)])),
            req.clone(),
            Mode::Confirm,
        )
        .await
        .unwrap_err();
        assert_eq!(e.name, "RegistryUndecodable", "{e}");
        // A key that is not the registry authority.
        let (other, other_path) = key_file("t4-o");
        let _ = other;
        req.registry_keypair = other_path.clone();
        let e = retire_version(&chain, &FakeScan(Ok(two_chains())), req, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(e.name, "NotRegistryAuthority", "{e}");
        assert_eq!(chain.sent_count(), 0, "every refusal sent nothing");
        files.iter().for_each(|p| remove(p));
        remove(&other_path);
    }

    #[tokio::test]
    async fn a_failed_send_says_how_many_entries_were_already_retired() {
        let (req, files, mut chain) = retire_setup("t5");
        chain.send_result = Err("blockhash expired".into());
        let e = retire_version(&chain, &FakeScan(Ok(two_chains())), req, Mode::Confirm)
            .await
            .unwrap_err();
        assert_eq!(e.name, "SendFailed", "{e}");
        assert!(e.detail.contains("0 of 3 entries were retired"), "{e}");
        files.iter().for_each(|p| remove(p));
    }

    #[test]
    fn base58_matches_the_reference_encoding() {
        // The filter the scan sends is the registry's magic, little endian.
        assert_eq!(base58(&registry::MAGIC.to_le_bytes()), "374yFw");
        assert_eq!(base58(&[0, 0, b'a']), "112g");
        assert_eq!(base58(b"hello world"), "StV1DL6CwTryKyV");
        assert_eq!(base58(&[]), "");
    }
}
