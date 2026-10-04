//! The deposit fields of the guest input, end to end over accounts: the batch account, its chunks and the
//! deposit records of the committed synthetic small batch
//! (`fixtures/prover-input/synthetic-deposits-small.{bin,json}`) are written as fixture accounts with the real
//! layout writers, read back through `fetch_and_verify_batch` and `fetch_deposit_records`, and built with
//! `build_batch_input`. The built input has to equal the committed `.bin` byte for byte, and its public values
//! the sidecar's.

use std::collections::HashMap;

use rome_zk_layouts::batch::{self, BatchDeposit, BatchFields};
use rome_zk_layouts::deposit_queue::deposit_record::{self, DepositRecordFields};
use rome_zk_layouts::exit::exit_config;
use rome_zk_prover_input::build::{build_batch_input, write_stdin_file, BuildError};
use rome_zk_prover_input::inbox::{AccountFetch, FetchError, InboxError};
use rome_zk_prover_input::verifier::{VerifierError, VerifierFetch};
use rome_zk_prover_input::{
    fetch_and_verify_batch, fetch_deposit_records, header_hash, DepositInput, RomePublicInput,
    RomeWitnessInput,
};
use solana_program::pubkey::Pubkey;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/prover-input/synthetic-deposits-small"
);

fn keccak() -> fn(&[&[u8]]) -> [u8; 32] {
    rome_zk_merkle::keccak256
}

fn unhex<const N: usize>(s: &str) -> [u8; N] {
    hex::decode(s).unwrap().try_into().unwrap()
}

/// The committed `.bin` (raw), its two decoded frames, and the sidecar JSON.
struct Small {
    raw: Vec<u8>,
    public: RomePublicInput,
    witness: RomeWitnessInput,
    sidecar: serde_json::Value,
}

fn small() -> Small {
    let raw = std::fs::read(format!("{FIXTURE}.bin")).unwrap();
    let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
    let public_bytes = &raw[8..8 + public_len];
    let rest = &raw[8 + public_len + (8 - public_len % 8) % 8..];
    let witness_len = u64::from_le_bytes(rest[0..8].try_into().unwrap()) as usize;
    let cfg = bincode::config::standard();
    let (public, _): (RomePublicInput, usize) =
        bincode::serde::decode_from_slice(public_bytes, cfg).unwrap();
    let (witness, _): (RomeWitnessInput, usize) =
        bincode::serde::decode_from_slice(&rest[8..8 + witness_len], cfg).unwrap();
    let sidecar =
        serde_json::from_str(&std::fs::read_to_string(format!("{FIXTURE}.json")).unwrap()).unwrap();
    Small {
        raw,
        public,
        witness,
        sidecar,
    }
}

/// Serves the fixture's own header, blocks and witnesses, so `build_batch_input` runs with no network.
struct FixtureVerifier<'a>(&'a Small);

impl VerifierFetch for FixtureVerifier<'_> {
    fn head(&mut self) -> Result<u64, VerifierError> {
        Ok(self.0.public.blocks.last().unwrap().header.number)
    }
    fn header(&mut self, number: u64) -> Result<alloy_consensus::Header, VerifierError> {
        assert_eq!(number + 1, self.0.public.blocks[0].header.number);
        Ok(self.0.public.parent_header.clone())
    }
    fn block(&mut self, number: u64) -> Result<reth_ethereum_primitives::Block, VerifierError> {
        Ok(self
            .0
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
    ) -> Result<alloy_rpc_types_debug::ExecutionWitness, VerifierError> {
        let idx = self
            .0
            .public
            .blocks
            .iter()
            .position(|b| b.header.number == number)
            .unwrap();
        Ok(self.0.witness.witnesses[idx].clone())
    }
}

#[derive(Default)]
struct FakeFetch {
    accounts: HashMap<Pubkey, Vec<u8>>,
}

impl AccountFetch for FakeFetch {
    fn get_account(&mut self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, FetchError> {
        Ok(self.accounts.get(pubkey).cloned())
    }
}

/// The small batch's range as its sidecar gives it.
fn range_of(s: &Small) -> BatchDeposit {
    let d = &s.sidecar["deposits"];
    BatchDeposit {
        from: d["deposit_from"].as_u64().unwrap(),
        to: d["deposit_to"].as_u64().unwrap(),
        hash_from: unhex(d["h_from"].as_str().unwrap()),
        hash_to: unhex(d["h_to"].as_str().unwrap()),
    }
}

/// The three deposit records of the small batch, as the bridge's record accounts: the settlement program, the
/// senders and the fields are the fixture's (the fixed settlement program and the seeded depositors), the
/// `hash_after` values are chained here with the deposit functions.
fn record_accounts(s: &Small, bridge: &Pubkey) -> Vec<(Pubkey, Vec<u8>)> {
    let range = range_of(s);
    let mut hash = range.hash_from;
    s.public
        .deposits
        .iter()
        .enumerate()
        .map(|(k, dep)| {
            let index = range.from + k as u64;
            let leaf = rome_zk_layouts::deposit::leaf(
                &keccak(),
                &s.public.settlement_program,
                s.public.chain_id,
                index,
                &dep.sender,
                &dep.recipient,
                dep.amount_gwei,
            );
            hash = rome_zk_layouts::deposit::chain_next(&keccak(), &hash, &leaf);
            let mut data = vec![0u8; deposit_record::LEN];
            deposit_record::write(
                &mut data,
                &DepositRecordFields {
                    index,
                    enqueue_unix_ts: 1_790_000_000,
                    sender: dep.sender,
                    recipient: dep.recipient,
                    amount_gwei: dep.amount_gwei,
                    hash_after: hash,
                },
            );
            let (pda, _) = deposit_record::pda(
                bridge,
                &s.public.settlement_program,
                s.public.chain_id,
                index,
            );
            (pda, data)
        })
        .collect()
}

fn exit_config_data(chain_id: u64, bridge_program: [u8; 32]) -> [u8; exit_config::LEN] {
    exit_config::write(&exit_config::ExitConfigFields {
        chain_id,
        exit_portal: [0; 20],
        bridge_program,
        pending_exit_portal: [0; 20],
        pending_bridge_program: [0; 32],
        pending_exit_cap: 0,
        pending_poster_bond: 0,
        activation_slot: 0,
        pending_mask: 0,
    })
}

struct Rig {
    inbox: Pubkey,
    bridge: Pubkey,
    fetch: FakeFetch,
}

/// The fixture accounts of the small batch: a finalized v3 batch account, its sealed chunks and its records.
fn rig(s: &Small) -> Rig {
    let inbox = Pubkey::new_unique();
    let bridge = Pubkey::new_unique();
    let settlement = Pubkey::new_from_array(s.public.settlement_program);
    let d = &s.sidecar["deposits"];
    let fields = BatchFields {
        chain_id: s.public.chain_id,
        batch: s.public.batch,
        open_slot: s.public.open_slot,
        expected_count: s.public.expected_count,
        leaves_present: s.public.expected_count,
        finalized: true,
        settlement_program: s.public.settlement_program,
        authority: [0; 32],
        root: unhex(d["inbox_root"].as_str().unwrap()),
        forced_root: unhex(d["forced_root"].as_str().unwrap()),
        acc: unhex(d["acc"].as_str().unwrap()),
        finalize_cursor: s.public.expected_count,
        open_unix_ts: s.public.open_unix_ts,
        deposit: Some(range_of(s)),
    };
    let mut fetch = FakeFetch::default();
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&inbox, &settlement, s.public.chain_id, s.public.batch);
    fetch
        .accounts
        .insert(batch_pda, batch::write_header_v3(&fields).unwrap().to_vec());
    for (idx, body) in s.public.chunk_bodies.iter().enumerate() {
        let header =
            rome_zk_layouts::chunk::write_header(&rome_zk_layouts::chunk::ChunkHeaderFields {
                authority: [0; 32],
                chain_id: s.public.chain_id,
                batch: s.public.batch,
                idx: idx as u32,
                len: body.len() as u32,
                sealed: true,
            });
        let mut data = header.to_vec();
        data.extend_from_slice(body);
        let (pda, _) = zk_inbox_client::chunk_pda(
            &inbox,
            &settlement,
            s.public.chain_id,
            s.public.batch,
            idx as u32,
        );
        fetch.accounts.insert(pda, data);
    }
    for (pda, data) in record_accounts(s, &bridge) {
        fetch.accounts.insert(pda, data);
    }
    let (exit_config_pda, _) = exit_config::pda(&settlement, s.public.chain_id);
    fetch.accounts.insert(
        exit_config_pda,
        exit_config_data(s.public.chain_id, bridge.to_bytes()).to_vec(),
    );
    Rig {
        inbox,
        bridge,
        fetch,
    }
}

fn read_back(
    rig: &mut Rig,
    s: &Small,
) -> Result<(zk_inbox_client::BatchAccount, Vec<Vec<u8>>), InboxError> {
    fetch_and_verify_batch(
        &mut rig.fetch,
        &rig.inbox,
        &Pubkey::new_from_array(s.public.settlement_program),
        s.public.chain_id,
        s.public.batch,
    )
}

/// The small batch built from accounts is the committed input, byte for byte, and its public values are the
/// sidecar's.
#[test]
fn small_batch_built_from_accounts_equals_the_committed_bin_and_sidecar() {
    let s = small();
    // The fixture uses the program tests' settlement program.
    #[cfg(feature = "synth")]
    assert_eq!(
        s.public.settlement_program,
        rome_zk_testkit::fixed_settlement_program_id().to_bytes()
    );
    let mut rig = rig(&s);
    let (account, chunk_bodies) = read_back(&mut rig, &s).unwrap();
    assert_eq!(account.deposit, Some(range_of(&s)));
    let deposits = fetch_deposit_records(&mut rig.fetch, &account).unwrap();
    assert_eq!(deposits, s.public.deposits);

    let (public, witness, expected) = build_batch_input(
        account,
        chunk_bodies,
        deposits,
        s.public.max_drift_secs,
        &mut FixtureVerifier(&s),
    )
    .unwrap();

    // The deposit fields come from the header and the records.
    assert_eq!(public.deposit_from, 0);
    assert_eq!(
        hex::encode(public.deposit_hash_from),
        s.sidecar["deposits"]["h_from"].as_str().unwrap()
    );
    assert_eq!(public.deposits.len(), 3);

    // Byte for byte against the committed file.
    let dir = std::env::temp_dir().join(format!(
        "rome-zk-prover-input-synth-small-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("built.bin");
    write_stdin_file(&path, &public, &witness).unwrap();
    let built = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(built, s.raw, "the built input is the committed .bin");

    // The expected public values are the sidecar's, field by field and as the 208 committed bytes.
    let e = &s.sidecar;
    assert_eq!(expected.chain_id, e["chain_id"].as_u64().unwrap());
    assert_eq!(expected.first_number, e["first_number"].as_u64().unwrap());
    assert_eq!(expected.last_number, e["last_number"].as_u64().unwrap());
    assert_eq!(expected.open_unix_ts, e["open_unix_ts"].as_u64().unwrap());
    assert_eq!(
        expected.max_drift_secs,
        e["max_drift_secs"].as_u64().unwrap()
    );
    assert_eq!(expected.gas_used, e["gas_used"].as_u64().unwrap());
    assert_eq!(expected.parent_hash, e["parent_hash"].as_str().unwrap());
    assert_eq!(
        expected.inbox_commitment,
        e["inbox_commitment"].as_str().unwrap()
    );
    assert_eq!(
        expected.forced_outcome_commitment,
        e["forced_outcome_commitment"].as_str().unwrap()
    );
    let last = public.blocks.last().unwrap();
    let pv = rome_zk_layouts::public_values::write(&rome_zk_layouts::public_values::PublicValues {
        chain_id: expected.chain_id,
        first_number: expected.first_number,
        last_number: expected.last_number,
        open_unix_ts: expected.open_unix_ts,
        max_drift_secs: expected.max_drift_secs,
        gas_used: expected.gas_used,
        parent_hash: unhex(&expected.parent_hash),
        last_block_hash: header_hash(&last.header).0,
        state_root: last.header.state_root.0,
        inbox_commitment: unhex(&expected.inbox_commitment),
        forced_outcome_commitment: unhex(&expected.forced_outcome_commitment),
    });
    assert_eq!(
        hex::encode(pv),
        e["public_values_hex"].as_str().unwrap(),
        "the public values equal the sidecar's"
    );
}

/// The reference commitment over the small batch's range is the sidecar's `acc` and `forced_root`.
#[test]
fn reference_commitment_with_deposits_over_the_small_range_equals_the_sidecar() {
    let s = small();
    let d = &s.sidecar["deposits"];
    let hashes: Vec<[u8; 32]> = s
        .public
        .chunk_bodies
        .iter()
        .map(|b| rome_zk_merkle::keccak256(&[b]))
        .collect();
    let (root, forced_root, acc) = zk_inbox_client::reference_commitment_with_deposits(
        s.public.chain_id,
        s.public.batch,
        s.public.open_slot,
        &hashes,
        &range_of(&s),
    );
    assert_eq!(hex::encode(root), d["inbox_root"].as_str().unwrap());
    assert_eq!(hex::encode(forced_root), d["forced_root"].as_str().unwrap());
    assert_eq!(hex::encode(acc), d["acc"].as_str().unwrap());
    // The empty-range function does not reproduce it: the range is part of `forced_root`.
    let (_, empty_forced, empty_acc) = zk_inbox_client::reference_commitment(
        s.public.chain_id,
        s.public.batch,
        s.public.open_slot,
        &hashes,
    );
    assert_ne!(empty_forced, forced_root);
    assert_ne!(empty_acc, acc);
}

/// A batch account whose range was changed after `acc` was committed does not verify.
#[test]
fn a_header_range_that_acc_does_not_cover_is_refused() {
    let s = small();
    let mut rig = rig(&s);
    let settlement = Pubkey::new_from_array(s.public.settlement_program);
    let (batch_pda, _) =
        zk_inbox_client::batch_pda(&rig.inbox, &settlement, s.public.chain_id, s.public.batch);
    let mut data = rig.fetch.accounts[&batch_pda].clone();
    data[batch::OFF_DEPOSIT_TO] = 2; // [0, 2) instead of [0, 3)
    rig.fetch.accounts.insert(batch_pda, data);
    assert!(matches!(
        read_back(&mut rig, &s),
        Err(InboxError::AccMismatch { .. })
    ));
}

fn account_of(rig: &Rig, s: &Small) -> zk_inbox_client::BatchAccount {
    let mut rig_fetch = FakeFetch {
        accounts: rig.fetch.accounts.clone(),
    };
    fetch_and_verify_batch(
        &mut rig_fetch,
        &rig.inbox,
        &Pubkey::new_from_array(s.public.settlement_program),
        s.public.chain_id,
        s.public.batch,
    )
    .unwrap()
    .0
}

fn record_pda(rig: &Rig, s: &Small, index: u64) -> Pubkey {
    deposit_record::pda(
        &rig.bridge,
        &s.public.settlement_program,
        s.public.chain_id,
        index,
    )
    .0
}

#[test]
fn a_record_that_does_not_follow_the_chain_is_refused() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);
    let pda = record_pda(&rig, &s, 1);
    let mut data = rig.fetch.accounts[&pda].clone();
    data[deposit_record::OFF_AMOUNT_GWEI] ^= 1;
    rig.fetch.accounts.insert(pda, data);
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::DepositRecordChainMismatch { index: 1, .. })
    ));
}

#[test]
fn a_missing_record_and_a_wrong_index_are_refused() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);

    // Record 2 stored under record 1's address.
    let (p1, p2) = (record_pda(&rig, &s, 1), record_pda(&rig, &s, 2));
    let saved = rig
        .fetch
        .accounts
        .insert(p1, rig.fetch.accounts[&p2].clone());
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::DepositRecordIndexMismatch {
            index: 1,
            got_index: 2
        })
    ));
    rig.fetch.accounts.insert(p1, saved.unwrap());

    // Record 2 absent.
    rig.fetch.accounts.remove(&p2);
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::DepositRecordMissing { index: 2, .. })
    ));
}

#[test]
fn build_refuses_records_that_are_not_the_headers_range() {
    let s = small();
    let mut rig = rig(&s);
    let (account, bodies) = read_back(&mut rig, &s).unwrap();
    let build = |deposits: Vec<DepositInput>| {
        build_batch_input(
            account.clone(),
            bodies.clone(),
            deposits,
            s.public.max_drift_secs,
            &mut FixtureVerifier(&s),
        )
        .map(|_| ())
    };
    assert!(matches!(
        build(vec![]),
        Err(BuildError::DepositCount {
            expected: 3,
            got: 0
        })
    ));
    let mut wrong = s.public.deposits.clone();
    wrong[2].amount_gwei += 1;
    assert!(matches!(
        build(wrong),
        Err(BuildError::DepositHashTo { .. })
    ));
    assert!(build(s.public.deposits.clone()).is_ok());

    // A v2 header (no range) takes no records.
    let mut v2 = account.clone();
    v2.deposit = None;
    assert!(matches!(
        build_batch_input(
            v2,
            bodies.clone(),
            s.public.deposits.clone(),
            s.public.max_drift_secs,
            &mut FixtureVerifier(&s)
        ),
        Err(BuildError::DepositCount {
            expected: 0,
            got: 3
        })
    ));
}

fn exit_config_pda_of(s: &Small) -> Pubkey {
    exit_config::pda(
        &Pubkey::new_from_array(s.public.settlement_program),
        s.public.chain_id,
    )
    .0
}

#[test]
fn a_missing_exit_config_is_refused() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);
    let pda = exit_config_pda_of(&s);
    rig.fetch.accounts.remove(&pda);
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::ExitConfigMissing { pda: p, .. }) if p == pda
    ));
}

#[test]
fn an_exit_config_with_a_zero_bridge_program_is_refused() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);
    let pda = exit_config_pda_of(&s);
    rig.fetch
        .accounts
        .insert(pda, exit_config_data(s.public.chain_id, [0; 32]).to_vec());
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::ExitConfigBridgeZero { pda: p }) if p == pda
    ));
}

#[test]
fn an_exit_config_for_another_chain_is_refused() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);
    let pda = exit_config_pda_of(&s);
    rig.fetch.accounts.insert(
        pda,
        exit_config_data(s.public.chain_id + 1, rig.bridge.to_bytes()).to_vec(),
    );
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::ExitConfigChainMismatch { got, expected, .. })
            if got == s.public.chain_id + 1 && expected == s.public.chain_id
    ));
}

/// The records are read under the bridge program the chain's `exit_config` names: pointing it elsewhere
/// finds no record there.
#[test]
fn the_records_are_read_under_the_bridge_the_exit_config_names() {
    let s = small();
    let mut rig = rig(&s);
    let account = account_of(&rig, &s);
    let pda = exit_config_pda_of(&s);
    rig.fetch.accounts.insert(
        pda,
        exit_config_data(s.public.chain_id, Pubkey::new_unique().to_bytes()).to_vec(),
    );
    assert!(matches!(
        fetch_deposit_records(&mut rig.fetch, &account),
        Err(InboxError::DepositRecordMissing { index: 0, .. })
    ));
}
