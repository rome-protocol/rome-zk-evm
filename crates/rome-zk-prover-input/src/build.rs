//! Orchestration: batch account + chunk bodies (Solana) → channel decode for the block
//! range → per-block fetch + witness (reth verifier) → the guest's two bincode inputs,
//! `ZiskStdin::write_slice`-framed, written to a file; and the expected public values, printed as hex so
//! a ziskemu run's committed output can be compared field by field.
//!
//! **Wire v3 deposit fields, empty for now:** `build_batch_input` writes the deposit-free range (the
//! batch's settlement program, `deposit_from` 0, `deposit_hash_from` = `h_0` of that program and chain,
//! no deposits). Real ranges come with the batch header v3 in the deposit build.
//!
//! **No `chain_config` here any more (wire v2):** the chain's rules are baked
//! into the guest ELF at compile time, not supplied by the host — `--genesis` (the CLI's own arg) is
//! still read (`crate::genesis::load_chain_config`) to learn `chain_id` for deriving PDAs, but its
//! `ChainConfig` value itself never reaches the wire any more.

use std::path::Path;

use crate::inbox::BatchAccount;
use crate::wire::{write_slice_frame, RomePublicInput, RomeWitnessInput};

/// `h_0` for `(settlement_program, chain_id)`: the deposit queue's hash chain before any deposit, and
/// the value a batch with an empty deposit range carries as `deposit_hash_from` (an empty range uses
/// the cursor's current value, which is `h_0` on a chain that never took a deposit).
fn empty_range_hash(settlement_program: &[u8; 32], chain_id: u64) -> [u8; 32] {
    let keccak = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
    rome_zk_layouts::deposit::queue_seed_hash(&keccak, settlement_program, chain_id)
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Inbox(#[from] crate::inbox::InboxError),
    #[error(transparent)]
    Genesis(#[from] crate::genesis::GenesisError),
    #[error(transparent)]
    Verifier(#[from] crate::verifier::VerifierError),
    #[error("write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// The expected public values (host-computed) printed alongside the written fixture, so a `ziskemu` run's
/// committed output can be checked field by field
/// against the host's expected values. `Deserialize` too: `--migrate-sidecar` decodes
/// an existing v1-shaped sidecar JSON straight into this struct — the same 9 fields, unrenamed, are the
/// whole of a v1 sidecar's shape, so decoding it as `ExpectedPublicValues` IS the migration's read side.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ExpectedPublicValues {
    pub chain_id: u64,
    pub first_number: u64,
    pub last_number: u64,
    pub open_unix_ts: u64,
    pub max_drift_secs: u64,
    pub gas_used: u64,
    pub parent_hash: String,
    pub inbox_commitment: String,
    pub forced_outcome_commitment: String,
}

/// Where this input fixture came from: recorded in the
/// sidecar alongside [`ExpectedPublicValues`] so a fixture on disk carries its own provenance rather than
/// relying on an operator's memory of which RPC/verifier/ELF build it was captured against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub solana_rpc: String,
    pub verifier_rpc: String,
    /// Unix timestamp (seconds since epoch, UTC) of when this fixture was built — no date-formatting
    /// dependency needed, and unambiguous.
    pub fetched_at: u64,
    /// The verifier's own `web3_clientVersion` string.
    pub verifier_version: String,
    /// The written `.bin` fixture's own size in bytes.
    pub input_bytes: u64,
    /// The guest ELF's sha256, when known — this tool does not build the ELF itself (`cargo-zisk
    /// build` does), so `None` unless the caller supplies one (`--elf-sha256`).
    pub elf_sha256: Option<String>,
    /// sha256 of the genesis file this fixture's `--genesis` argument named — the
    /// same fact `guest-rome`'s `build.rs` prints as a build warning for the ELF that embeds it, so a
    /// sidecar and the ELF it was captured against can be cross-checked against one shared genesis hash.
    pub genesis_sha256: String,
    /// The follower's own prove-attempt counter this sidecar was captured under:
    /// the follower loop's `Provenance` records the REAL winning attempt of the run that wrote this
    /// sidecar, so a later resume can record every event under that same attempt rather than a literal
    /// `1` — a cache is keyed by the identity of the TARGET, and here the target is one row
    /// of history, `(chain_id, batch, attempt)`. Standalone runs of this crate's own CLI (never part of a
    /// follower loop) have no attempt counter of their own and write `0`.
    pub attempt: u32,
}

/// The full sidecar this tool writes alongside the `.bin` fixture: the host-computed expected public
/// values, `last_block_hash`/`state_root` (only known after a real guest execution — `None` here, filled
/// in by whoever runs `ziskemu`/`cargo-zisk prove` against this fixture next), and [`Provenance`].
///
/// `Deserialize` too: the follower loop's own resume gate
/// (`rome-zk-prover::follower::try_resume`) reads this exact shape back off disk to decide whether a
/// cached proof is still trustworthy — never a stripped-down, crate-private sidecar shape of its own.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Sidecar {
    #[serde(flatten)]
    pub expected: ExpectedPublicValues,
    pub last_block_hash: Option<String>,
    pub state_root: Option<String>,
    pub provenance: Provenance,
}

/// Builds the two guest input frames for one finalized batch and returns them alongside the expected
/// public values the guest should commit (everything except `last_block_hash`/`state_root`, which need
/// the verifier's own execution to know — printed as `pending` until a real `ziskemu` run fills them in).
///
/// `verifier` is every reth-verifier read this function needs (no `Provider` trait
/// object any more — every fetch is a blocking `ureq` POST under the real
/// [`crate::verifier::RemoteVerifier`], so this function itself stays synchronous; the CLI binary's
/// own `tokio::runtime::Runtime` exists only for the Solana RPC client), abstracted behind
/// [`crate::verifier::VerifierFetch`] so a follower loop can drive this whole function
/// against a fake in its own tests, never a live reth node.
pub fn build_batch_input(
    batch_account: BatchAccount,
    chunk_bodies: Vec<Vec<u8>>,
    max_drift_secs: u64,
    verifier: &mut impl crate::verifier::VerifierFetch,
) -> Result<(RomePublicInput, RomeWitnessInput, ExpectedPublicValues), BuildError> {
    let (first, last) = crate::inbox::decode_block_range(&chunk_bodies)?;

    let parent_header = verifier.header(first - 1)?;
    let mut blocks = Vec::with_capacity((last - first + 1) as usize);
    let mut witnesses = Vec::with_capacity(blocks.capacity());
    let mut gas_used: u64 = 0;
    for number in first..=last {
        let block = verifier.block(number)?;
        gas_used = gas_used.saturating_add(block.header.gas_used);
        blocks.push(block);
        witnesses.push(verifier.witness(number)?);
    }

    let parent_hash = crate::header_hash(&parent_header);

    let settlement_program = batch_account.settlement_program.to_bytes();
    let public = RomePublicInput {
        chain_id: batch_account.chain_id,
        batch: batch_account.batch,
        open_slot: batch_account.open_slot,
        open_unix_ts: batch_account.open_unix_ts,
        max_drift_secs,
        expected_count: batch_account.expected_count,
        chunk_bodies,
        parent_header,
        blocks,
        settlement_program,
        deposit_from: 0,
        deposit_hash_from: empty_range_hash(&settlement_program, batch_account.chain_id),
        deposits: vec![],
    };
    let witness = RomeWitnessInput { witnesses };

    let expected = ExpectedPublicValues {
        chain_id: batch_account.chain_id,
        first_number: first,
        last_number: last,
        open_unix_ts: batch_account.open_unix_ts as u64,
        max_drift_secs,
        gas_used,
        parent_hash: hex::encode(parent_hash),
        inbox_commitment: hex::encode(batch_account.acc),
        forced_outcome_commitment: hex::encode(batch_account.forced_root),
    };

    Ok((public, witness, expected))
}

/// Writes the two frames in `ZiskStdin::write_slice`'s own framing, concatenated in the order the guest
/// reads them (public, then witness) — one `.bin` file, matching `guest-rome`'s README.
pub fn write_stdin_file(
    path: &Path,
    public: &RomePublicInput,
    witness: &RomeWitnessInput,
) -> Result<(), BuildError> {
    let mut out = Vec::new();
    write_slice_frame(&mut out, &public.serialize());
    write_slice_frame(&mut out, &witness.serialize());
    std::fs::write(path, out).map_err(|source| BuildError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// Decodes an existing sidecar JSON's own `ExpectedPublicValues` fields — a v1-shaped
/// sidecar's WHOLE shape is exactly these 9 fields, and a v2 sidecar's own `expected` fields are
/// flattened at the top level too (`Sidecar`'s `#[serde(flatten)]`), so this same decode works against
/// either a genuinely-old sidecar or a v2 one being re-migrated. Extra top-level keys (`provenance`,
/// `last_block_hash`, `state_root` on an already-v2 file) are ignored by `serde_json`'s default
/// deny-unknown-fields-off behavior — this function's job is only to recover the batch's own public
/// values, never to validate the rest of the envelope.
pub fn decode_expected_public_values(
    existing_json: &str,
) -> Result<ExpectedPublicValues, serde_json::Error> {
    serde_json::from_str(existing_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidecar serializes with `provenance` as a nested object and
    /// every `ExpectedPublicValues` field flattened alongside it (not nested under `expected`) — the
    /// shape a sidecar reader (the report, or a future CLI diff tool) expects.
    #[test]
    fn sidecar_serializes_with_flattened_expected_values_and_nested_provenance() {
        let sidecar = Sidecar {
            expected: ExpectedPublicValues {
                chain_id: 200101,
                first_number: 39181,
                last_number: 39190,
                open_unix_ts: 1_789_356_237,
                max_drift_secs: 60,
                gas_used: 0,
                parent_hash: "0xaa".into(),
                inbox_commitment: "0xbb".into(),
                forced_outcome_commitment: "0xcc".into(),
            },
            last_block_hash: None,
            state_root: None,
            provenance: Provenance {
                solana_rpc: "https://example.invalid".into(),
                verifier_rpc: "http://127.0.0.1:18547".into(),
                fetched_at: 1_789_999_999,
                verifier_version: "reth/v1.2.3".into(),
                input_bytes: 1234,
                elf_sha256: None,
                genesis_sha256: "0xdd".into(),
                attempt: 1,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&sidecar).unwrap();
        assert_eq!(v["chain_id"], 200101);
        assert_eq!(v["provenance"]["verifier_version"], "reth/v1.2.3");
        assert_eq!(v["provenance"]["input_bytes"], 1234);
        assert_eq!(v["provenance"]["genesis_sha256"], "0xdd");
        assert!(v["last_block_hash"].is_null());
        assert!(
            v.get("expected").is_none(),
            "expected fields must be flattened, not nested"
        );
    }

    /// A genuinely v1-shaped sidecar (the exact 9-field, no-`provenance`
    /// shape the committed `txv1-dev-batch-3930.json` had before this fix) decodes as
    /// [`ExpectedPublicValues`] — the read side of `--migrate-sidecar`, exercised with none of the CLI
    /// plumbing around it.
    #[test]
    fn decode_expected_public_values_reads_a_v1_shaped_sidecar() {
        let v1_json = r#"{
            "chain_id": 200101,
            "first_number": 39181,
            "last_number": 39190,
            "open_unix_ts": 1789356237,
            "max_drift_secs": 60,
            "gas_used": 0,
            "parent_hash": "86580145cb3b31f3b8970a1b329bace24a75e7a1921356cfc01c15dd67160be5",
            "inbox_commitment": "a15a9874c2ba39ce0c083ee8f590edc44bbac3c1c7ec6643982c0de5b0e2e3b9",
            "forced_outcome_commitment": "a939698ea5a2cb2ee272e900f2f3d986294007dcb635b1846b597aec642e938c"
        }"#;
        let expected = decode_expected_public_values(v1_json).unwrap();
        assert_eq!(expected.chain_id, 200101);
        assert_eq!(expected.first_number, 39181);
        assert_eq!(expected.last_number, 39190);
        assert_eq!(
            expected.forced_outcome_commitment,
            "a939698ea5a2cb2ee272e900f2f3d986294007dcb635b1846b597aec642e938c"
        );
    }

    /// A v1-shaped sidecar missing a required field (a truncated/corrupted fixture) is refused, not
    /// silently defaulted.
    #[test]
    fn decode_expected_public_values_refuses_a_missing_field() {
        let truncated = r#"{"chain_id": 200101}"#;
        assert!(decode_expected_public_values(truncated).is_err());
    }

    /// The written stdin file's two frames decode back to the same public input (a pure
    /// round trip through the on-disk `.bin` format, no network involved).
    #[test]
    fn write_stdin_file_round_trips() {
        let public = RomePublicInput {
            chain_id: 200101,
            batch: 1,
            open_slot: 1,
            open_unix_ts: 1,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![9, 9, 9]],
            parent_header: alloy_consensus::Header::default(),
            blocks: vec![],
            settlement_program: [0x33; 32],
            deposit_from: 0,
            deposit_hash_from: [0; 32],
            deposits: vec![],
        };
        let witness = RomeWitnessInput { witnesses: vec![] };
        // Unique per process/run, not a fixed shared name — see genesis.rs's own test for why (a stale
        // dir from an earlier run on a persistent self-hosted runner can be unwritable).
        let dir = std::env::temp_dir().join(format!(
            "rome-zk-prover-input-test-build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.bin");
        write_stdin_file(&path, &public, &witness).unwrap();

        let raw = std::fs::read(&path).unwrap();
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_bytes = &raw[8..8 + public_len];
        let (decoded, _): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded.chain_id, public.chain_id);
        assert_eq!(decoded.chunk_bodies, public.chunk_bodies);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serves the committed batch-3930 fixture's own header, blocks and witnesses, so
    /// `build_batch_input` runs end to end with no network.
    struct FixtureVerifier {
        public: RomePublicInput,
        witness: RomeWitnessInput,
    }

    impl crate::verifier::VerifierFetch for FixtureVerifier {
        fn head(&mut self) -> Result<u64, crate::verifier::VerifierError> {
            Ok(self.public.blocks.last().unwrap().header.number)
        }
        fn header(
            &mut self,
            number: u64,
        ) -> Result<alloy_consensus::Header, crate::verifier::VerifierError> {
            assert_eq!(number + 1, self.public.blocks[0].header.number);
            Ok(self.public.parent_header.clone())
        }
        fn block(
            &mut self,
            number: u64,
        ) -> Result<reth_ethereum_primitives::Block, crate::verifier::VerifierError> {
            Ok(self
                .public
                .blocks
                .iter()
                .find(|b| b.header.number == number)
                .unwrap()
                .clone())
        }
        fn witness(
            &mut self,
            number: u64,
        ) -> Result<alloy_rpc_types_debug::ExecutionWitness, crate::verifier::VerifierError>
        {
            let idx = self
                .public
                .blocks
                .iter()
                .position(|b| b.header.number == number)
                .unwrap();
            Ok(self.witness.witnesses[idx].clone())
        }
    }

    /// `build_batch_input` writes the empty deposit range: the batch account's settlement program,
    /// `deposit_from` 0, `h_0` of (that program, the chain id), no deposits, and every other field as
    /// before. Run against the committed batch-3930 fixture, it reproduces that file's public and witness
    /// frames byte for byte, which also ties the fixture's migrated tail to what the builder writes.
    #[test]
    fn build_batch_input_writes_the_empty_deposit_range_and_reproduces_the_fixture() {
        use std::str::FromStr;

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-batch-3930.bin"
        );
        let raw = std::fs::read(path).unwrap();
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_bytes = &raw[8..8 + public_len];
        let witness_frame = &raw[8 + public_len + ((8 - public_len % 8) % 8)..];
        let (fixture_public, _): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard()).unwrap();
        let witness_len = u64::from_le_bytes(witness_frame[0..8].try_into().unwrap()) as usize;
        let (fixture_witness, _): (RomeWitnessInput, usize) = bincode::serde::decode_from_slice(
            &witness_frame[8..8 + witness_len],
            bincode::config::standard(),
        )
        .unwrap();

        let settlement_program = solana_program::pubkey::Pubkey::from_str(
            "6yWj1Az1JmHBmt1654bFx2UdPMWd6Aak2QqBDPQxpj56",
        )
        .unwrap();
        let batch_account = BatchAccount {
            chain_id: fixture_public.chain_id,
            batch: fixture_public.batch,
            open_slot: fixture_public.open_slot,
            expected_count: fixture_public.expected_count,
            leaves_present: fixture_public.expected_count,
            finalized: true,
            settlement_program,
            authority: solana_program::pubkey::Pubkey::new_unique(),
            root: [0; 32],
            forced_root: [0; 32],
            acc: [0; 32],
            finalize_cursor: 0,
            open_unix_ts: fixture_public.open_unix_ts,
            deposit: None,
        };
        let mut verifier = FixtureVerifier {
            public: fixture_public.clone(),
            witness: fixture_witness,
        };
        let (public, witness, _expected) = build_batch_input(
            batch_account,
            fixture_public.chunk_bodies.clone(),
            fixture_public.max_drift_secs,
            &mut verifier,
        )
        .unwrap();

        assert_eq!(public.settlement_program, settlement_program.to_bytes());
        assert_eq!(public.deposit_from, 0);
        assert_eq!(
            public.deposit_hash_from,
            empty_range_hash(&settlement_program.to_bytes(), 200101)
        );
        assert!(public.deposits.is_empty());

        assert_eq!(public.serialize(), public_bytes);
        let mut witness_out = Vec::new();
        write_slice_frame(&mut witness_out, &witness.serialize());
        assert_eq!(witness_out, witness_frame);
    }
}
