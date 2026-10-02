//! `rome-zk-prover-input --batch <id> --solana-rpc <url> --inbox <id> --settlement <id> --verifier-rpc
//! <url> --genesis <path> --out <file>`: read-only against a live devnet + reth
//! verifier. Writes the guest's two bincode inputs and prints `first, last, gas_used, expected public
//! values (hex)`. Never writes to any cluster; never generates a proof.
//!
//! `--migrate-sidecar <existing.json>`: upgrades an already-committed sidecar to the v2
//! shape (`provenance`, `last_block_hash`, `state_root`) with **no network access at all** — it reads the
//! existing sidecar's own `expected` fields (the batch's public values never change under a migration;
//! only the sidecar's own envelope does) and fills in the v2-only fields from explicit CLI values, then
//! writes back through the SAME [`rome_zk_prover_input::build::Sidecar`] the live path uses — one
//! definition of the v2 shape, never a hand-typed JSON literal that could drift from it.

use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use solana_program::pubkey::Pubkey;

use rome_zk_prover_input::inbox::AccountFetch;

#[derive(Parser)]
struct Args {
    /// Migrate this existing sidecar JSON to the v2 shape instead of fetching a fresh fixture — no
    /// network access. Requires `--fetched-at`, `--verifier-version`, `--input-bytes`, `--solana-rpc`,
    /// `--verifier-rpc` (recorded as provenance labels, not connected to) and `--genesis` (hashed for
    /// `provenance.genesis_sha256`, never fetched over the network either — a local file read).
    /// `--last-block-hash`/`--state-root` are optional here too, same as the live path: `None` for
    /// whichever a real guest execution has not (yet) confirmed.
    #[arg(long)]
    migrate_sidecar: Option<PathBuf>,

    #[arg(long, required_unless_present = "migrate_sidecar")]
    batch: Option<u64>,
    #[arg(long)]
    solana_rpc: Option<String>,
    #[arg(long, required_unless_present = "migrate_sidecar")]
    inbox: Option<String>,
    #[arg(long, required_unless_present = "migrate_sidecar")]
    settlement: Option<String>,
    #[arg(long)]
    verifier_rpc: Option<String>,
    #[arg(long)]
    genesis: PathBuf,
    #[arg(long)]
    out: PathBuf,
    /// The guest ELF's sha256, if known (this tool does not build the ELF itself — `cargo-zisk build`
    /// does). Recorded verbatim in the sidecar's `provenance.elf_sha256`; omitted
    /// (`None`) when not supplied.
    #[arg(long)]
    elf_sha256: Option<String>,

    // --migrate-sidecar-only fields — the migration has no live guest/verifier run to
    // read these from, so they are explicit, named CLI values rather than silently defaulted.
    /// The real ELF's committed `last_block_hash` for this batch (only known from a real guest
    /// execution — never re-derived here).
    #[arg(long)]
    last_block_hash: Option<String>,
    /// The real ELF's committed `state_root` for this batch (same source as `--last-block-hash`).
    #[arg(long)]
    state_root: Option<String>,
    /// When the `.bin` fixture being migrated was ORIGINALLY fetched (unix seconds) — carried forward
    /// from that original run, never "now" (migrating a sidecar's envelope does not re-fetch anything).
    #[arg(long)]
    fetched_at: Option<u64>,
    /// The reth verifier's `web3_clientVersion` string from the original fetch.
    #[arg(long)]
    verifier_version: Option<String>,
    /// The `.bin` fixture's own size in bytes (`ls -la`/`stat`, not re-measured here since this mode
    /// never touches the `.bin` file).
    #[arg(long)]
    input_bytes: Option<u64>,
}

/// Adapts a live `solana_client::nonblocking::rpc_client::RpcClient` to
/// [`rome_zk_prover_input::inbox::AccountFetch`] — one account at a time via [`AccountFetch::get_account`],
/// or paged `getMultipleAccounts` (`rome_zk_prover_input::inbox::MAX_ACCOUNTS_PER_GET_MULTIPLE`
/// keys/call — mirroring `rome-zk-derive::inbox::InboxRetrieval`'s own fix) via
/// [`AccountFetch::get_multiple_accounts`], which `fetch_and_verify_batch` now uses for every chunk read.
struct RpcFetch<'a> {
    rt: &'a tokio::runtime::Runtime,
    client: solana_client::nonblocking::rpc_client::RpcClient,
}

impl AccountFetch for RpcFetch<'_> {
    /// `get_account_with_commitment` reports a genuinely missing
    /// account as `Ok(None)` directly rather than an error a caller would have to string-match — never
    /// conflated with the RPC call itself failing, which is `Err(FetchError)` and never a panic.
    fn get_account(
        &mut self,
        pubkey: &Pubkey,
    ) -> Result<Option<Vec<u8>>, rome_zk_prover_input::inbox::FetchError> {
        self.rt.block_on(async {
            self.client
                .get_account_with_commitment(pubkey, self.client.commitment())
                .await
                .map(|resp| resp.value.map(|a| a.data))
                .map_err(|e| rome_zk_prover_input::inbox::FetchError(e.to_string()))
        })
    }

    fn get_multiple_accounts(
        &mut self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Vec<u8>>>, rome_zk_prover_input::inbox::FetchError> {
        self.rt.block_on(async {
            let mut out = Vec::with_capacity(pubkeys.len());
            for page in pubkeys.chunks(rome_zk_prover_input::inbox::MAX_ACCOUNTS_PER_GET_MULTIPLE) {
                let accounts = self.client.get_multiple_accounts(page).await.map_err(|e| {
                    rome_zk_prover_input::inbox::FetchError(format!(
                        "getMultipleAccounts failed for a page of {} keys: {e}",
                        page.len()
                    ))
                })?;
                out.extend(accounts.into_iter().map(|a| a.map(|a| a.data)));
            }
            Ok(out)
        })
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(existing_sidecar_path) = &args.migrate_sidecar {
        return migrate_sidecar(&args, existing_sidecar_path);
    }

    let rt = tokio::runtime::Runtime::new()?;

    let inbox_program = Pubkey::from_str(
        args.inbox
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--inbox is required without --migrate-sidecar"))?,
    )?;
    let settlement_program =
        Pubkey::from_str(args.settlement.as_deref().ok_or_else(|| {
            anyhow::anyhow!("--settlement is required without --migrate-sidecar")
        })?)?;
    let batch = args
        .batch
        .ok_or_else(|| anyhow::anyhow!("--batch is required without --migrate-sidecar"))?;
    let solana_rpc = args
        .solana_rpc
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--solana-rpc is required without --migrate-sidecar"))?;
    let verifier_rpc = args
        .verifier_rpc
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--verifier-rpc is required without --migrate-sidecar"))?;

    let chain_config = rome_zk_prover_input::genesis::load_chain_config(&args.genesis)?;
    let chain_id = chain_config.chain_id;
    let genesis_sha256 = rome_zk_prover_input::genesis::genesis_sha256(&args.genesis)?;

    let client = solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
        solana_rpc.clone(),
        solana_commitment_config::CommitmentConfig::finalized(),
    );
    let mut fetch = RpcFetch { rt: &rt, client };

    let (batch_account, chunk_bodies) = rome_zk_prover_input::inbox::fetch_and_verify_batch(
        &mut fetch,
        &inbox_program,
        chain_id,
        batch,
    )?;
    println!(
        "batch {batch} verified: acc {} matches the on-chain batch account",
        hex::encode(batch_account.acc)
    );

    let (chain_config_pda, _) =
        zk_settlement_client::chain_config_pda(&settlement_program, chain_id);
    let chain_config_data = fetch
        .get_account(&chain_config_pda)
        .map_err(|e| anyhow::anyhow!("chain_config account {chain_config_pda} fetch: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("chain_config account {chain_config_pda} not found"))?;
    let chain_config_account =
        zk_settlement_client::decode_chain_config_account(&chain_config_data)?;
    let max_drift_secs = chain_config_account.max_drift_secs.ok_or_else(|| {
        anyhow::anyhow!("chain_config is v1 (no max_drift_secs) — MigrateChainV2 first")
    })?;

    // Wire v2: `chain_config` never reaches the guest input any more — `--genesis` is
    // still read above (for `chain_id`), but its value stops here.
    let mut verifier_fetch = rome_zk_prover_input::verifier::RemoteVerifier { url: &verifier_rpc };
    let (public, witness, expected) = rome_zk_prover_input::build::build_batch_input(
        batch_account,
        chunk_bodies,
        max_drift_secs,
        &mut verifier_fetch,
    )?;

    rome_zk_prover_input::build::write_stdin_file(&args.out, &public, &witness)?;
    let input_bytes = std::fs::metadata(&args.out)?.len();
    println!(
        "wrote {} ({} blocks {}..={}, {input_bytes} bytes)",
        args.out.display(),
        public.blocks.len(),
        expected.first_number,
        expected.last_number
    );

    let verifier_version = rome_zk_prover_input::verifier::fetch_client_version(&verifier_rpc)?;
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sidecar_body = rome_zk_prover_input::build::Sidecar {
        expected,
        // Only known after a real guest execution (`ziskemu`/`cargo-zisk prove`) — this tool never
        // proves (module doc) and never touches a cluster, so both stay `None` here.
        last_block_hash: None,
        state_root: None,
        provenance: rome_zk_prover_input::build::Provenance {
            solana_rpc,
            verifier_rpc,
            fetched_at,
            verifier_version,
            input_bytes,
            elf_sha256: args.elf_sha256.clone(),
            genesis_sha256,
            // This tool never runs as part of a follower loop (module doc) — `0` is the same
            // "not one specific attempt" sentinel the follower's own loop-level halts use.
            attempt: 0,
        },
    };
    println!("sidecar: {}", serde_json::to_string_pretty(&sidecar_body)?);

    let sidecar_path = args.out.with_extension("json");
    std::fs::write(&sidecar_path, serde_json::to_string_pretty(&sidecar_body)?)?;
    println!("wrote {}", sidecar_path.display());

    Ok(())
}

/// Upgrades `existing_sidecar_path` (a v1-shaped, 9-field sidecar) to the v2
/// [`rome_zk_prover_input::build::Sidecar`] shape — no network access. The nine `expected` fields are
/// read straight off the existing JSON (a batch's public values do not change under a sidecar-envelope
/// migration); every v2-only field comes from an explicit `--*` CLI value, never a default.
fn migrate_sidecar(args: &Args, existing_sidecar_path: &std::path::Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(existing_sidecar_path)?;
    let existing =
        rome_zk_prover_input::build::decode_expected_public_values(&raw).map_err(|e| {
            anyhow::anyhow!(
                "{existing_sidecar_path:?}: does not decode as the expected-public-values fields \
             (chain_id/first_number/last_number/open_unix_ts/max_drift_secs/gas_used/parent_hash/\
             inbox_commitment/forced_outcome_commitment): {e}"
            )
        })?;

    let genesis_sha256 = rome_zk_prover_input::genesis::genesis_sha256(&args.genesis)?;

    let sidecar_body = rome_zk_prover_input::build::Sidecar {
        expected: existing,
        // Both stay whatever `--last-block-hash`/`--state-root` supplied — `None` when a real guest
        // execution has not (yet) confirmed a given field, exactly as the live-fetch path already
        // leaves them; a migration is not required to have both in hand to still upgrade the envelope
        // (provenance, the newly-added `genesis_sha256`) for the fields it DOES know.
        last_block_hash: args.last_block_hash.clone(),
        state_root: args.state_root.clone(),
        provenance: rome_zk_prover_input::build::Provenance {
            solana_rpc: args.solana_rpc.clone().ok_or_else(|| {
                anyhow::anyhow!("--solana-rpc is required with --migrate-sidecar")
            })?,
            verifier_rpc: args.verifier_rpc.clone().ok_or_else(|| {
                anyhow::anyhow!("--verifier-rpc is required with --migrate-sidecar")
            })?,
            fetched_at: args.fetched_at.ok_or_else(|| {
                anyhow::anyhow!("--fetched-at is required with --migrate-sidecar")
            })?,
            verifier_version: args.verifier_version.clone().ok_or_else(|| {
                anyhow::anyhow!("--verifier-version is required with --migrate-sidecar")
            })?,
            input_bytes: args.input_bytes.ok_or_else(|| {
                anyhow::anyhow!("--input-bytes is required with --migrate-sidecar")
            })?,
            elf_sha256: args.elf_sha256.clone(),
            genesis_sha256,
            // A v1-shaped sidecar predates this field entirely — `0`, the same "not one specific
            // attempt" sentinel a standalone (non-follower) run of this tool always writes.
            attempt: 0,
        },
    };

    let out = serde_json::to_string_pretty(&sidecar_body)?;
    println!("migrated sidecar: {out}");
    std::fs::write(&args.out, &out)?;
    println!("wrote {}", args.out.display());
    Ok(())
}
