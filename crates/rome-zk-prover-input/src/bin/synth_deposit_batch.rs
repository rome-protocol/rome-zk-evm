//! `synth-deposit-batch`: builds a synthetic batch that carries deposits, as a wire v3 guest input plus a JSON
//! sidecar of the expected public values, so the guest's deposit rules can be run under the zkVM emulator without a
//! live chain.
//!
//! **How the blocks are made.** A local reth node (peer discovery off, no peers; the caller starts it) is driven the
//! way the derivation node drives its own: `testing_buildBlockV1` builds each block from payload attributes
//! (the header rule is `rome_zk_executor_api::canonical_header_rule_with_withdrawals`, and each block's withdrawals
//! are its slice of the deposits, `deposit_withdrawal`), then `engine_newPayloadV4` and `engine_forkchoiceUpdatedV3`
//! make it canonical. The blocks and their execution witnesses are then fetched back with this crate's `verifier`
//! module, exactly as the real input generator does.
//!
//! **What is computed here.** The deposit records, their hash chain, `forced_root` and `acc` come from
//! `rome_zk_layouts::deposit` (and `rome_zk_layouts::acc`). The channel stream is encoded with
//! `rome_zk_channel::set_deposits_end` and cut with `cut_frames`; the chunk bodies are those frames.
//!
//! **Determinism.** Every key, address and amount derives from a fixed seed. The settlement program is
//! `rome_zk_testkit::fixed_settlement_program_id()` and each sender is the pubkey of a keypair made from a fixed seed
//! (`rome_zk_testkit::synthetic_depositor_keypair`), so program tests can sign `Deposit` and reproduce this batch's
//! hash chain. The seeds are public test values; no address here is funded. Block timestamps are a fixed base plus the block
//! number, so the same node build produces the same blocks.
//!
//! The two shapes: `small` is 3 deposits over 2 blocks with an empty block between them (blocks 1..=3, counts 2, 0,
//! 1); `full` is 4 deposits in each of 60 blocks, 240 in all.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Address, B256};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use clap::{Parser, ValueEnum};
use hmac::{Hmac, Mac};
use rome_zk_channel::{
    cut_frames, encode_stream, resolve_deposits_end, set_deposits_end, Block as StreamBlock, Frame,
    DEFAULT_MAX_FRAME_BODY_LEN,
};
use rome_zk_executor_api::{
    canonical_header_rule_with_withdrawals, deposit_withdrawal, withdrawals_root,
};
use rome_zk_layouts::deposit::{self, DepositRecord};
use rome_zk_layouts::public_values::{self, PublicValues};
use rome_zk_prover_input::build::{write_stdin_file, Provenance, Sidecar};
use rome_zk_prover_input::verifier::{fetch_client_version, RemoteVerifier, VerifierFetch};
use rome_zk_prover_input::{header_hash, DepositInput, RomePublicInput, RomeWitnessInput};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Block `n` of the synthetic chain has timestamp `BASE_TIMESTAMP + n`.
const BASE_TIMESTAMP: u64 = 1_790_000_000;
const BATCH: u64 = 1;
const OPEN_SLOT: u64 = 1_000;
const MAX_DRIFT_SECS: u64 = 60;
/// The batch opens this long after its last block was sealed (an honest batch: the guest's drift bound is one-sided).
const OPEN_AFTER_LAST_BLOCK_SECS: u64 = 5;
/// The example chain's fee recipient (its genesis coinbase).
const FEE_RECIPIENT: Address = Address::ZERO;

const RECIPIENT_LABEL: &[u8] = b"synthetic-deposits/v1/recipient";

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Shape {
    Small,
    Full,
}

impl Shape {
    /// How many deposits each block carries, in block order.
    fn per_block_counts(self) -> Vec<usize> {
        match self {
            Shape::Small => vec![2, 0, 1],
            Shape::Full => vec![4; 60],
        }
    }
}

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    rome_zk_merkle::keccak256(parts)
}

/// The synthetic settlement program id: the one the program tests load the settlement program under
/// (`rome_zk_testkit::fixed_settlement_program_id()`), so a program test reproduces this batch's hash chain.
fn settlement_program() -> [u8; 32] {
    rome_zk_testkit::fixed_settlement_program_id().to_bytes()
}

/// Deposit `index`'s record, derived from fixed seeds. Nothing here is secret: the sender is the pubkey of the
/// synthetic depositor `index` (`rome_zk_testkit::synthetic_depositor_pubkey`, a keypair made from a fixed seed, so a
/// program test can sign `Deposit` as it), the recipient a 20-byte address nobody holds a key for, the amount a fixed
/// function of the index.
fn record(index: u64) -> DepositRecord {
    let sender = rome_zk_testkit::synthetic_depositor_pubkey(index);
    let r = keccak(&[RECIPIENT_LABEL, &index.to_le_bytes()]);
    let mut recipient = [0u8; 20];
    recipient.copy_from_slice(&r[12..]);
    DepositRecord {
        sender,
        recipient,
        amount_gwei: 1_000_000 + 12_345 * index,
    }
}

/// The deposits of a shape, in queue order, and the cumulative count at the end of each block.
fn plan(shape: Shape) -> (Vec<DepositRecord>, Vec<u64>) {
    let counts = shape.per_block_counts();
    let total: usize = counts.iter().sum();
    let records: Vec<DepositRecord> = (0..total as u64).map(record).collect();
    let mut ends = Vec::with_capacity(counts.len());
    let mut running = 0u64;
    for c in &counts {
        running += *c as u64;
        ends.push(running);
    }
    (records, ends)
}

/// Block `k`'s withdrawals: its slice of the deposits (`[from_end, to_end)`), one withdrawal per deposit.
fn block_withdrawals(records: &[DepositRecord], from: u64, to: u64) -> Vec<Withdrawal> {
    (from..to)
        .map(|i| {
            let r = &records[i as usize];
            deposit_withdrawal(i, Address::from(r.recipient), r.amount_gwei)
        })
        .collect()
}

// ---- the commitments, as a pure function of the wire input ---------------------------------------------------------

/// Every deposit-derived value of a batch, recomputed from nothing but the wire input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Commitments {
    h_0: [u8; 32],
    h_from: [u8; 32],
    h_to: [u8; 32],
    deposit_to: u64,
    deposits_commitment: [u8; 32],
    forced_tx_commitment: [u8; 32],
    forced_root: [u8; 32],
    inbox_root: [u8; 32],
    acc: [u8; 32],
}

fn commitments(public: &RomePublicInput) -> Commitments {
    let h = keccak as fn(&[&[u8]]) -> [u8; 32];
    let sp = &public.settlement_program;
    let records: Vec<DepositRecord> = public
        .deposits
        .iter()
        .map(|d| DepositRecord {
            sender: d.sender,
            recipient: d.recipient,
            amount_gwei: d.amount_gwei,
        })
        .collect();
    let from = public.deposit_from;
    let to = from + records.len() as u64;
    let h_0 = deposit::queue_seed_hash(&h, sp, public.chain_id);
    let h_from = public.deposit_hash_from;
    let h_to = deposit::chain_through(&h, sp, public.chain_id, from, &h_from, &records);
    let forced_root = deposit::forced_root(&h, from, to, &h_from, &h_to);

    let chunk_hashes: Vec<[u8; 32]> = public.chunk_bodies.iter().map(|b| keccak(&[b])).collect();
    let leaves: Vec<[u8; 32]> = chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, c)| rome_zk_merkle::indexed_leaf(&h, i as u32, c))
        .collect();
    let inbox_root = rome_zk_merkle::root(&h, &leaves);
    let acc = rome_zk_layouts::acc(
        &h,
        public.chain_id,
        public.batch,
        public.open_slot,
        public.expected_count,
        &inbox_root,
        &forced_root,
    );
    Commitments {
        h_0,
        h_from,
        h_to,
        deposit_to: to,
        deposits_commitment: deposit::deposits_commitment(&h, from, to, &h_from, &h_to),
        forced_tx_commitment: deposit::forced_tx_empty_commitment(&h),
        forced_root,
        inbox_root,
        acc,
    }
}

/// The 208-byte public values the guest should commit for this input, given the executed last block's header fields.
fn expected_public_values(public: &RomePublicInput, c: &Commitments) -> PublicValues {
    let last = public.blocks.last().expect("a batch has blocks");
    PublicValues {
        chain_id: public.chain_id,
        first_number: public.blocks[0].header.number,
        last_number: last.header.number,
        open_unix_ts: public.open_unix_ts as u64,
        max_drift_secs: public.max_drift_secs,
        gas_used: public.blocks.iter().map(|b| b.header.gas_used).sum(),
        parent_hash: header_hash(&public.parent_header).0,
        last_block_hash: header_hash(&last.header).0,
        state_root: last.header.state_root.0,
        inbox_commitment: c.acc,
        forced_outcome_commitment: c.forced_root,
    }
}

// ---- the sidecar ---------------------------------------------------------------------------------------------------

/// One block's row in the sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BlockRow {
    number: u64,
    timestamp: u64,
    /// How many deposits this block carries.
    deposits: u64,
    /// The cumulative deposit count after this block (the value the block's fifth stream field carries, or inherits).
    deposits_end: u64,
    withdrawals_root: String,
    block_hash: String,
}

/// The intermediate values of the deposit range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DepositSection {
    settlement_program: String,
    batch: u64,
    open_slot: u64,
    /// The number of chunk bodies (frames) the stream was cut into.
    expected_count: u32,
    deposit_from: u64,
    deposit_to: u64,
    h_0: String,
    h_from: String,
    h_to: String,
    deposits_commitment: String,
    forced_tx_commitment: String,
    forced_root: String,
    inbox_root: String,
    acc: String,
    blocks: Vec<BlockRow>,
}

/// The sidecar this tool writes: the ordinary [`Sidecar`] (its nine expected fields, `last_block_hash`, `state_root`
/// and `provenance`), the whole 208-byte public values as hex, and the deposit range's intermediate values.
#[derive(Debug, Serialize, Deserialize)]
struct SyntheticSidecar {
    #[serde(flatten)]
    sidecar: Sidecar,
    /// The 208 bytes the guest commits, hex.
    public_values_hex: String,
    deposits: DepositSection,
}

fn block_rows(public: &RomePublicInput, ends: &[u64]) -> Vec<BlockRow> {
    let mut prev = public.deposit_from;
    public
        .blocks
        .iter()
        .zip(ends)
        .map(|(b, &end)| {
            let root = withdrawals_root(
                b.body
                    .withdrawals
                    .as_ref()
                    .map(|w| w.as_slice())
                    .unwrap_or(&[]),
            );
            let row = BlockRow {
                number: b.header.number,
                timestamp: b.header.timestamp,
                deposits: end - prev,
                deposits_end: end,
                withdrawals_root: hex::encode(root),
                block_hash: hex::encode(header_hash(&b.header)),
            };
            prev = end;
            row
        })
        .collect()
}

fn build_sidecar(
    public: &RomePublicInput,
    ends: &[u64],
    provenance: Provenance,
) -> SyntheticSidecar {
    let c = commitments(public);
    let pv = expected_public_values(public, &c);
    let pv_bytes = public_values::write(&pv);
    let sidecar = Sidecar {
        expected: rome_zk_prover_input::build::ExpectedPublicValues {
            chain_id: pv.chain_id,
            first_number: pv.first_number,
            last_number: pv.last_number,
            open_unix_ts: pv.open_unix_ts,
            max_drift_secs: pv.max_drift_secs,
            gas_used: pv.gas_used,
            parent_hash: hex::encode(pv.parent_hash),
            inbox_commitment: hex::encode(pv.inbox_commitment),
            forced_outcome_commitment: hex::encode(pv.forced_outcome_commitment),
        },
        last_block_hash: Some(hex::encode(pv.last_block_hash)),
        state_root: Some(hex::encode(pv.state_root)),
        provenance,
    };
    SyntheticSidecar {
        sidecar,
        public_values_hex: hex::encode(pv_bytes),
        deposits: DepositSection {
            settlement_program: hex::encode(public.settlement_program),
            batch: public.batch,
            open_slot: public.open_slot,
            expected_count: public.expected_count,
            deposit_from: public.deposit_from,
            deposit_to: c.deposit_to,
            h_0: hex::encode(c.h_0),
            h_from: hex::encode(c.h_from),
            h_to: hex::encode(c.h_to),
            deposits_commitment: hex::encode(c.deposits_commitment),
            forced_tx_commitment: hex::encode(c.forced_tx_commitment),
            forced_root: hex::encode(c.forced_root),
            inbox_root: hex::encode(c.inbox_root),
            acc: hex::encode(c.acc),
            blocks: block_rows(public, ends),
        },
    }
}

// ---- the node ------------------------------------------------------------------------------------------------------

/// A JSON-RPC endpoint, optionally with the Engine API's JWT.
struct Rpc {
    url: String,
    jwt_secret: Option<Vec<u8>>,
}

impl Rpc {
    fn token(secret: &[u8]) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_secs();
        let claims = URL_SAFE_NO_PAD.encode(format!("{{\"iat\":{iat}}}"));
        let signing_input = format!("{header}.{claims}");
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts a key of any length");
        mac.update(signing_input.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut req = ureq::post(&self.url)
            .timeout(Duration::from_secs(120))
            .set("content-type", "application/json");
        if let Some(secret) = &self.jwt_secret {
            req = req.set("authorization", &format!("Bearer {}", Self::token(secret)));
        }
        let response: serde_json::Value = req
            .send_json(body)
            .map_err(|e| anyhow!("{method} at {}: {e}", self.url))?
            .into_json()
            .with_context(|| format!("{method}: malformed JSON response"))?;
        if let Some(err) = response.get("error") {
            bail!("{method} returned an error: {err}");
        }
        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

fn hex_quantity(n: u64) -> String {
    format!("0x{n:x}")
}

fn withdrawal_json(w: &Withdrawal) -> serde_json::Value {
    serde_json::json!({
        "index": hex_quantity(w.index),
        "validatorIndex": hex_quantity(w.validator_index),
        "address": format!("{:#x}", w.address),
        "amount": hex_quantity(w.amount),
    })
}

/// Builds block `number` on `parent` and makes it canonical, the derivation node's route: `testing_buildBlockV1` on
/// the plain RPC, then `engine_newPayloadV4` and `engine_forkchoiceUpdatedV3` on the authenticated Engine API.
/// Returns the new block's hash.
#[allow(clippy::too_many_arguments)]
fn build_block(
    rpc: &Rpc,
    engine: &Rpc,
    chain_id: u64,
    parent: B256,
    number: u64,
    timestamp: u64,
    gas_limit: u64,
    withdrawals: &[Withdrawal],
) -> Result<B256> {
    let rule = canonical_header_rule_with_withdrawals(chain_id, number, FEE_RECIPIENT, withdrawals);
    let attrs = serde_json::json!({
        "timestamp": hex_quantity(timestamp),
        "prevRandao": format!("{:#x}", rule.prev_randao),
        "suggestedFeeRecipient": format!("{:#x}", rule.beneficiary),
        "withdrawals": withdrawals.iter().map(withdrawal_json).collect::<Vec<_>>(),
        "parentBeaconBlockRoot": format!("{:#x}", rule.parent_beacon_block_root),
        "targetGasLimit": hex_quantity(gas_limit),
    });
    let envelope = rpc.call(
        "testing_buildBlockV1",
        serde_json::json!([format!("{parent:#x}"), attrs, Vec::<String>::new(), null]),
    )?;
    let payload = envelope
        .get("executionPayload")
        .ok_or_else(|| anyhow!("block {number}: no executionPayload in the build response"))?
        .clone();
    let requests = envelope
        .get("executionRequests")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));

    let field = |name: &str| -> Result<String> {
        payload
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("block {number}: payload has no {name}"))
    };
    let hash: B256 = field("blockHash")?.parse()?;
    if field("parentHash")?.parse::<B256>()? != parent
        || field("blockNumber")? != hex_quantity(number)
        || field("timestamp")? != hex_quantity(timestamp)
        || field("gasLimit")? != hex_quantity(gas_limit)
    {
        bail!("block {number}: the built payload's parent, number, timestamp or gas limit differ from the request");
    }
    let got = payload
        .get("withdrawals")
        .and_then(|w| w.as_array())
        .map_or(0, |w| w.len());
    if got != withdrawals.len() {
        bail!(
            "block {number}: built with {got} withdrawals, expected {}",
            withdrawals.len()
        );
    }

    let status = engine.call(
        "engine_newPayloadV4",
        serde_json::json!([
            payload,
            Vec::<String>::new(),
            format!("{:#x}", rule.parent_beacon_block_root),
            requests
        ]),
    )?;
    if status["status"] != "VALID" {
        bail!("block {number}: engine_newPayloadV4 returned {status}");
    }
    let state = serde_json::json!({
        "headBlockHash": format!("{hash:#x}"),
        "safeBlockHash": format!("{hash:#x}"),
        "finalizedBlockHash": format!("{hash:#x}"),
    });
    let fcu = engine.call(
        "engine_forkchoiceUpdatedV3",
        serde_json::json!([state, null]),
    )?;
    if fcu["payloadStatus"]["status"] != "VALID" {
        bail!("block {number}: engine_forkchoiceUpdatedV3 returned {fcu}");
    }
    Ok(hash)
}

fn peer_count(rpc: &Rpc) -> Result<u64> {
    let v = rpc.call("net_peerCount", serde_json::json!([]))?;
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("net_peerCount: not a string: {v}"))?;
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

// ---- the batch -----------------------------------------------------------------------------------------------------

/// Runs the whole build against a node that holds only its genesis block.
fn generate(
    shape: Shape,
    rpc_url: &str,
    engine_url: &str,
    jwt_secret: Vec<u8>,
    genesis: &Path,
    fetched_at: u64,
) -> Result<(RomePublicInput, RomeWitnessInput, SyntheticSidecar)> {
    let chain_id = rome_zk_prover_input::genesis::load_chain_config(genesis)?.chain_id;
    let genesis_sha256 = rome_zk_prover_input::genesis::genesis_sha256(genesis)?;
    let rpc = Rpc {
        url: rpc_url.to_owned(),
        jwt_secret: None,
    };
    let engine = Rpc {
        url: engine_url.to_owned(),
        jwt_secret: Some(jwt_secret),
    };

    // The hard rule for every node we start: no peers, ever.
    if peer_count(&rpc)? != 0 {
        bail!("the node has peers (net_peerCount is not 0): refusing to build on a node that joined a network");
    }
    let mut verifier = RemoteVerifier { url: rpc_url };
    if verifier.head()? != 0 {
        bail!(
            "the node is not at genesis (head {}): start a fresh node",
            verifier.head()?
        );
    }
    let parent_header = verifier.header(0)?;
    let gas_limit = parent_header.gas_limit;
    let (records, ends) = plan(shape);

    // 1. Build every block through the node.
    let mut parent = header_hash(&parent_header);
    let mut prev_end = 0u64;
    for (k, &end) in ends.iter().enumerate() {
        let number = k as u64 + 1;
        let withdrawals = block_withdrawals(&records, prev_end, end);
        parent = build_block(
            &rpc,
            &engine,
            chain_id,
            parent,
            number,
            BASE_TIMESTAMP + number,
            gas_limit,
            &withdrawals,
        )?;
        prev_end = end;
    }
    if peer_count(&rpc)? != 0 {
        bail!("the node has peers after the build (net_peerCount is not 0)");
    }

    // 2. Fetch the blocks and witnesses back, the way the real input generator does, and check each block against
    //    the rule it was built under.
    let n = ends.len() as u64;
    let mut blocks = Vec::with_capacity(ends.len());
    let mut witnesses = Vec::with_capacity(ends.len());
    let mut prev_end = 0u64;
    for number in 1..=n {
        let end = ends[(number - 1) as usize];
        let withdrawals = block_withdrawals(&records, prev_end, end);
        let rule =
            canonical_header_rule_with_withdrawals(chain_id, number, FEE_RECIPIENT, &withdrawals);
        let block = verifier.block(number)?;
        let h = &block.header;
        if h.number != number
            || h.timestamp != BASE_TIMESTAMP + number
            || h.gas_limit != gas_limit
            || h.mix_hash != rule.prev_randao
            || h.beneficiary != rule.beneficiary
            || h.withdrawals_root != Some(rule.withdrawals_root)
            || h.parent_beacon_block_root != Some(rule.parent_beacon_block_root)
            || !block.body.transactions.is_empty()
            || block
                .body
                .withdrawals
                .as_ref()
                .map(|w| w.to_vec())
                .unwrap_or_default()
                != withdrawals
        {
            bail!("block {number}: the fetched block does not match the rule it was built under");
        }
        witnesses.push(verifier.witness(number)?);
        blocks.push(block);
        prev_end = end;
    }

    // 3. The stream and the frames.
    let from = 0u64;
    let mut stream: Vec<StreamBlock> = blocks
        .iter()
        .map(|b| StreamBlock {
            number: b.header.number,
            timestamp: b.header.timestamp,
            gas_limit: b.header.gas_limit,
            txs: vec![],
            deposits_end: None,
        })
        .collect();
    set_deposits_end(&mut stream, from, &ends)?;
    let compressed = encode_stream(&stream);
    let frames = cut_frames(chain_id, BATCH, &compressed, DEFAULT_MAX_FRAME_BODY_LEN);
    let chunk_bodies: Vec<Vec<u8>> = frames.iter().map(Frame::to_bytes).collect();

    // 4. The wire input.
    let sp = settlement_program();
    let h = keccak as fn(&[&[u8]]) -> [u8; 32];
    let last_ts = blocks.last().expect("a batch has blocks").header.timestamp;
    let public = RomePublicInput {
        chain_id,
        batch: BATCH,
        open_slot: OPEN_SLOT,
        open_unix_ts: (last_ts + OPEN_AFTER_LAST_BLOCK_SECS) as i64,
        max_drift_secs: MAX_DRIFT_SECS,
        expected_count: chunk_bodies.len() as u32,
        chunk_bodies,
        parent_header,
        blocks,
        settlement_program: sp,
        deposit_from: from,
        deposit_hash_from: deposit::queue_seed_hash(&h, &sp, chain_id),
        deposits: records
            .iter()
            .map(|r| DepositInput {
                sender: r.sender,
                recipient: r.recipient,
                amount_gwei: r.amount_gwei,
            })
            .collect(),
    };
    let witness = RomeWitnessInput { witnesses };

    // The stream must end at `to`.
    let blocks_back = rome_zk_channel::decode_stream(&compressed)?;
    let resolved = resolve_deposits_end(&blocks_back, from)?;
    if resolved != ends || resolved.last().copied() != Some(from + records.len() as u64) {
        bail!("the encoded stream does not resolve to the planned deposit cursor");
    }

    let provenance = Provenance {
        solana_rpc: "none (a synthetic batch: no Solana read)".into(),
        verifier_rpc: "a local reth node: peer discovery off, no peers".into(),
        fetched_at,
        verifier_version: fetch_client_version(rpc_url)?,
        input_bytes: 0, // set once the file is written
        elf_sha256: None,
        genesis_sha256,
        attempt: 0,
    };
    let sidecar = build_sidecar(&public, &ends, provenance);
    Ok((public, witness, sidecar))
}

// ---- reading a written input back ----------------------------------------------------------------------------------

/// Splits the two `write_slice` frames of a `.bin` and decodes them.
#[cfg(test)]
fn read_stdin_file(raw: &[u8]) -> Result<(RomePublicInput, RomeWitnessInput)> {
    fn frame(raw: &[u8]) -> Result<(&[u8], &[u8])> {
        let len = u64::from_le_bytes(
            raw.get(0..8)
                .ok_or_else(|| anyhow!("short frame"))?
                .try_into()?,
        ) as usize;
        let payload = raw
            .get(8..8 + len)
            .ok_or_else(|| anyhow!("frame overruns the file"))?;
        let padded = len + (8 - len % 8) % 8;
        Ok((
            payload,
            raw.get(8 + padded..)
                .ok_or_else(|| anyhow!("padding overruns the file"))?,
        ))
    }
    let (public_bytes, rest) = frame(raw)?;
    let (witness_bytes, tail) = frame(rest)?;
    if !tail.is_empty() {
        bail!("{} trailing bytes after the witness frame", tail.len());
    }
    let cfg = bincode::config::standard();
    let (public, used): (RomePublicInput, usize) =
        bincode::serde::decode_from_slice(public_bytes, cfg)?;
    if used != public_bytes.len() {
        bail!("trailing bytes inside the public frame");
    }
    let (witness, used): (RomeWitnessInput, usize) =
        bincode::serde::decode_from_slice(witness_bytes, cfg)?;
    if used != witness_bytes.len() {
        bail!("trailing bytes inside the witness frame");
    }
    Ok((public, witness))
}

// ---- the command line ----------------------------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "synth-deposit-batch",
    about = "Builds a synthetic deposit batch (wire v3 input + JSON sidecar) from a fresh local reth node"
)]
struct Cli {
    /// Which batch to build.
    #[arg(long, value_enum)]
    shape: Shape,
    /// The genesis file the node was started with.
    #[arg(long)]
    genesis: PathBuf,
    /// The node's plain JSON-RPC endpoint (must serve eth, net, debug and testing).
    #[arg(long, default_value = "http://127.0.0.1:18547")]
    rpc: String,
    /// The node's authenticated Engine API endpoint.
    #[arg(long, default_value = "http://127.0.0.1:18551")]
    engine: String,
    /// The file holding the node's Engine API JWT secret, hex (generated per run, never committed).
    #[arg(long)]
    jwt_secret: PathBuf,
    /// Where to write the `.bin`; the sidecar goes beside it with a `.json` extension.
    #[arg(long)]
    out: PathBuf,
    /// Unix seconds recorded as `provenance.fetched_at`.
    #[arg(long)]
    fetched_at: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let secret_hex = std::fs::read_to_string(&cli.jwt_secret)
        .with_context(|| format!("read {}", cli.jwt_secret.display()))?;
    let secret = hex::decode(secret_hex.trim().trim_start_matches("0x"))
        .context("the JWT secret is not hex")?;
    let (public, witness, mut sidecar) = generate(
        cli.shape,
        &cli.rpc,
        &cli.engine,
        secret,
        &cli.genesis,
        cli.fetched_at,
    )?;
    write_stdin_file(&cli.out, &public, &witness)?;
    sidecar.sidecar.provenance.input_bytes = std::fs::metadata(&cli.out)?.len();
    let json = cli.out.with_extension("json");
    std::fs::write(&json, serde_json::to_string_pretty(&sidecar)? + "\n")?;
    eprintln!(
        "wrote {} ({} bytes) and {}",
        cli.out.display(),
        sidecar.sidecar.provenance.input_bytes,
        json.display()
    );
    Ok(())
}

// ---- tests ---------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signer;

    fn fixture(name: &str) -> (RomePublicInput, RomeWitnessInput, SyntheticSidecar, Vec<u8>) {
        let base = format!(
            "{}/../../fixtures/prover-input/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read(format!("{base}.bin")).unwrap();
        let (public, witness) = read_stdin_file(&raw).unwrap();
        let sidecar: SyntheticSidecar =
            serde_json::from_str(&std::fs::read_to_string(format!("{base}.json")).unwrap())
                .unwrap();
        (public, witness, sidecar, raw)
    }

    fn unhex<const N: usize>(s: &str) -> [u8; N] {
        hex::decode(s).unwrap().try_into().unwrap()
    }

    /// Decodes a fixture's `.bin`, recomputes every hash value of its sidecar with the deposit functions, and checks
    /// the stream, the withdrawals and the public values against it.
    fn check(name: &str, shape: Shape) {
        let (public, witness, sidecar, raw) = fixture(name);
        let d = &sidecar.deposits;
        let c = commitments(&public);

        // The hash values in the sidecar are the ones the deposit functions give for the input.
        assert_eq!(hex::encode(c.h_0), d.h_0);
        assert_eq!(hex::encode(c.h_from), d.h_from);
        assert_eq!(
            c.h_from, c.h_0,
            "both batches start the queue, so h_from is h_0"
        );
        assert_eq!(hex::encode(c.h_to), d.h_to);
        assert_eq!(c.deposit_to, d.deposit_to);
        assert_eq!(hex::encode(c.deposits_commitment), d.deposits_commitment);
        assert_eq!(hex::encode(c.forced_tx_commitment), d.forced_tx_commitment);
        assert_eq!(hex::encode(c.forced_root), d.forced_root);
        assert_eq!(hex::encode(c.inbox_root), d.inbox_root);
        assert_eq!(hex::encode(c.acc), d.acc);
        assert_eq!(hex::encode(public.settlement_program), d.settlement_program);
        assert_eq!(public.expected_count, d.expected_count);
        assert_eq!(public.chunk_bodies.len() as u32, d.expected_count);
        assert_ne!(
            c.forced_root,
            rome_zk_layouts::forced_empty_root(&(keccak as fn(&[&[u8]]) -> [u8; 32])),
            "a deposit batch's forced_root is not the empty constant"
        );

        // The public values: 208 bytes, equal to the sidecar's, and to its flattened fields.
        let pv = expected_public_values(&public, &c);
        let bytes = public_values::write(&pv);
        assert_eq!(bytes.len(), 208);
        assert_eq!(hex::encode(bytes), sidecar.public_values_hex);
        assert_eq!(public_values::read(&bytes).unwrap(), pv);
        let e = &sidecar.sidecar.expected;
        assert_eq!(e.chain_id, pv.chain_id);
        assert_eq!(e.first_number, pv.first_number);
        assert_eq!(e.last_number, pv.last_number);
        assert_eq!(e.open_unix_ts, pv.open_unix_ts);
        assert_eq!(e.max_drift_secs, pv.max_drift_secs);
        assert_eq!(e.gas_used, pv.gas_used);
        assert_eq!(e.parent_hash, hex::encode(pv.parent_hash));
        assert_eq!(e.inbox_commitment, hex::encode(c.acc));
        assert_eq!(e.forced_outcome_commitment, hex::encode(c.forced_root));
        assert_eq!(
            sidecar.sidecar.last_block_hash,
            Some(hex::encode(pv.last_block_hash))
        );
        assert_eq!(sidecar.sidecar.state_root, Some(hex::encode(pv.state_root)));
        assert_eq!(sidecar.sidecar.provenance.input_bytes, raw.len() as u64);

        // The stream: it decodes, and its deposit cursor ends at `to`.
        let frames: Vec<Frame> = public
            .chunk_bodies
            .iter()
            .map(|b| Frame::from_bytes(b).unwrap())
            .collect();
        let compressed = rome_zk_channel::reassemble(&frames).unwrap();
        let stream = rome_zk_channel::decode_stream(&compressed).unwrap();
        let ends = resolve_deposits_end(&stream, public.deposit_from).unwrap();
        assert_eq!(ends.len(), public.blocks.len());
        assert_eq!(
            *ends.last().unwrap(),
            c.deposit_to,
            "resolve_deposits_end over the stream ends at to"
        );
        assert_eq!(
            c.deposit_to,
            public.deposit_from + public.deposits.len() as u64
        );
        assert_eq!(
            ends,
            d.blocks.iter().map(|r| r.deposits_end).collect::<Vec<_>>()
        );
        // The canonical encoder: the field is present exactly where the cursor changes.
        let mut prev = public.deposit_from;
        for (b, &end) in stream.iter().zip(&ends) {
            assert_eq!(b.deposits_end, (end != prev).then_some(end));
            prev = end;
        }
        // The frames are what `cut_frames` makes of the stream.
        let again = cut_frames(
            public.chain_id,
            public.batch,
            &compressed,
            DEFAULT_MAX_FRAME_BODY_LEN,
        );
        assert_eq!(again, frames);

        // Every block: stream header fields equal the header, the withdrawals are its slice of the deposits, and
        // the header's withdrawals root is their root and the sidecar's.
        assert_eq!(stream.len(), public.blocks.len());
        assert_eq!(witness.witnesses.len(), public.blocks.len());
        assert_eq!(d.blocks.len(), public.blocks.len());
        let mut parent = header_hash(&public.parent_header);
        let mut prev = public.deposit_from;
        for (i, block) in public.blocks.iter().enumerate() {
            let h = &block.header;
            assert_eq!(
                h.parent_hash, parent,
                "block {} links to its parent",
                h.number
            );
            parent = header_hash(h);
            assert_eq!(stream[i].number, h.number);
            assert_eq!(stream[i].timestamp, h.timestamp);
            assert_eq!(stream[i].gas_limit, h.gas_limit);
            assert!(stream[i].txs.is_empty() && block.body.transactions.is_empty());
            let want: Vec<Withdrawal> = (prev..ends[i])
                .map(|idx| {
                    let r = &public.deposits[(idx - public.deposit_from) as usize];
                    deposit_withdrawal(idx, Address::from(r.recipient), r.amount_gwei)
                })
                .collect();
            let got = block
                .body
                .withdrawals
                .as_ref()
                .map(|w| w.to_vec())
                .unwrap_or_default();
            assert_eq!(got, want, "block {} withdrawals", h.number);
            let root = withdrawals_root(&want);
            assert_eq!(h.withdrawals_root, Some(root));
            assert_eq!(d.blocks[i].withdrawals_root, hex::encode(root));
            assert_eq!(d.blocks[i].number, h.number);
            assert_eq!(d.blocks[i].timestamp, h.timestamp);
            assert_eq!(d.blocks[i].deposits, ends[i] - prev);
            assert_eq!(d.blocks[i].block_hash, hex::encode(header_hash(h)));
            prev = ends[i];
        }

        // The shape.
        let counts: Vec<u64> = d.blocks.iter().map(|r| r.deposits).collect();
        let want_counts: Vec<u64> = shape.per_block_counts().iter().map(|&c| c as u64).collect();
        assert_eq!(counts, want_counts);
        assert_eq!(
            public.deposits.len(),
            shape.per_block_counts().iter().sum::<usize>()
        );
        // The records are the seeded ones.
        for (i, r) in public.deposits.iter().enumerate() {
            let want = record(i as u64);
            assert_eq!(
                (r.sender, r.recipient, r.amount_gwei),
                (want.sender, want.recipient, want.amount_gwei)
            );
        }
    }

    /// The golden values the program tests rebuild: the fixtures use the program tests' settlement program id and
    /// senders that are the pubkeys of the fixed-seed depositor keypairs, and their h_n, forced_root and acc are
    /// pinned here. A program test that signs `Deposit` as `synthetic_depositor_keypair(i)` against
    /// `fixed_settlement_program_id()` must reproduce these.
    #[test]
    fn fixtures_use_the_program_test_ids_and_pin_their_golden_values() {
        let golden = [
            (
                "synthetic-deposits-small",
                3u64,
                "de2ada9842a4ee1472aaff3f3e50339efaf3c2d922a3aae2d84b4df87eb25fd9",
                "8bb7e79bf9767f0cc2772f2e2ee2c8d389cbdf8e87138488c738c50aeff3be04",
                "e92a3f30c5aa39e79c7b849892f81036a0e15090e81dcd2cab4357dd89843843",
            ),
            (
                "synthetic-deposits-full",
                240,
                "2db06acee8508913890616a9964418378bc9ae60ad2f4fd5fd7b443b94f6c186",
                "73c86aad38f065ed34253cca3936796709bcc86cc0c135fc8f89625a40f04523",
                "6e024c6f4b0e5fd7b677e7c76cf198f32a9505bbaedfff08f92e69816d3329e4",
            ),
        ];
        for (name, n, h_n, forced_root, acc) in golden {
            let (public, _, sidecar, _) = fixture(name);
            assert_eq!(
                public.settlement_program,
                rome_zk_testkit::fixed_settlement_program_id().to_bytes()
            );
            assert_eq!(public.deposits.len() as u64, n);
            for (i, d) in public.deposits.iter().enumerate() {
                assert_eq!(
                    d.sender,
                    rome_zk_testkit::synthetic_depositor_keypair(i as u64)
                        .pubkey()
                        .to_bytes(),
                    "{name}: deposit {i}'s sender"
                );
            }
            let c = commitments(&public);
            assert_eq!(hex::encode(c.h_to), h_n, "{name}: h_{n}");
            assert_eq!(
                hex::encode(c.forced_root),
                forced_root,
                "{name}: forced_root"
            );
            assert_eq!(hex::encode(c.acc), acc, "{name}: acc");
            assert_eq!(sidecar.deposits.h_to, h_n);
            assert_eq!(sidecar.deposits.forced_root, forced_root);
            assert_eq!(sidecar.deposits.acc, acc);
        }
    }

    #[test]
    fn small_fixture_rederives_from_its_bin() {
        check("synthetic-deposits-small", Shape::Small);
        let (public, _, sidecar, _) = fixture("synthetic-deposits-small");
        assert_eq!(public.deposits.len(), 3);
        assert_eq!(public.blocks.len(), 3);
        assert_eq!(
            sidecar
                .deposits
                .blocks
                .iter()
                .map(|r| r.deposits)
                .collect::<Vec<_>>(),
            vec![2, 0, 1]
        );
        // The empty block between the two deposit blocks has no withdrawals and the empty root.
        assert!(public.blocks[1]
            .body
            .withdrawals
            .as_ref()
            .is_none_or(|w| w.is_empty()));
        assert_eq!(
            public.blocks[1].header.withdrawals_root,
            Some(alloy_consensus::constants::EMPTY_WITHDRAWALS)
        );
    }

    #[test]
    fn full_fixture_rederives_from_its_bin() {
        check("synthetic-deposits-full", Shape::Full);
        let (public, _, _, _) = fixture("synthetic-deposits-full");
        assert_eq!(public.deposits.len(), 240);
        assert_eq!(public.blocks.len(), 60);
    }

    /// The small fixture's chain value, recomputed from explicit preimages (a 126-byte leaf and a 64-byte step) without
    /// the deposit functions, so it does not rest on them alone.
    #[test]
    fn small_fixture_hash_chain_matches_explicit_preimages() {
        let (public, _, sidecar, _) = fixture("synthetic-deposits-small");
        let mut h = keccak(&[
            b"rome-zk/deposit-queue/v1",
            &public.settlement_program,
            &public.chain_id.to_le_bytes(),
        ]);
        assert_eq!(hex::encode(h), sidecar.deposits.h_0);
        for (i, d) in public.deposits.iter().enumerate() {
            let mut pre = Vec::new();
            pre.extend_from_slice(b"rome-zk/deposit/v1");
            pre.extend_from_slice(&public.settlement_program);
            pre.extend_from_slice(&public.chain_id.to_le_bytes());
            pre.extend_from_slice(&(i as u64).to_le_bytes());
            pre.extend_from_slice(&d.sender);
            pre.extend_from_slice(&d.recipient);
            pre.extend_from_slice(&d.amount_gwei.to_le_bytes());
            assert_eq!(pre.len(), 126);
            let leaf = keccak(&[&pre]);
            h = keccak(&[&h, &leaf]);
        }
        assert_eq!(hex::encode(h), sidecar.deposits.h_to);
        let dc = keccak(&[
            b"rome-zk/forced/deposits/v1",
            &0u64.to_le_bytes(),
            &3u64.to_le_bytes(),
            &unhex::<32>(&sidecar.deposits.h_from),
            &h,
        ]);
        assert_eq!(hex::encode(dc), sidecar.deposits.deposits_commitment);
        let ftx = keccak(&[b"rome-zk/forced/txs/empty/v1"]);
        let forced = keccak(&[b"rome-zk/forced/v2", &dc, &ftx]);
        assert_eq!(hex::encode(forced), sidecar.deposits.forced_root);
    }

    /// Planning is a pure function of the shape: the counts, the cursor and the seeded records.
    #[test]
    fn plan_is_deterministic_and_has_the_specified_shapes() {
        let (r1, e1) = plan(Shape::Small);
        assert_eq!((r1.len(), e1.clone()), (3, vec![2, 2, 3]));
        let (r2, e2) = plan(Shape::Full);
        assert_eq!(r2.len(), 240);
        assert_eq!(e2.len(), 60);
        assert_eq!(e2.last(), Some(&240));
        assert!(e2.windows(2).all(|w| w[1] - w[0] == 4));
        assert_eq!(plan(Shape::Small).0, r1);
        // Seeds, not secrets: records differ by index, and the same index gives the same record.
        assert_ne!(record(0), record(1));
        assert_eq!(record(7), record(7));
        // The settlement program and the senders are the ones a program test can use.
        assert_eq!(settlement_program(), [0x51u8; 32]);
        for i in [0u64, 1, 239] {
            let kp = rome_zk_testkit::synthetic_depositor_keypair(i);
            assert_eq!(record(i).sender, kp.pubkey().to_bytes());
        }
    }

    /// The JWT is a three-part HS256 token whose header and signature decode and verify.
    #[test]
    fn jwt_token_is_hs256_and_verifies() {
        let secret = [7u8; 32];
        let t = Rpc::token(&secret);
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[0]).unwrap(),
            br#"{"alg":"HS256","typ":"JWT"}"#
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap())
            .unwrap();
    }
}
