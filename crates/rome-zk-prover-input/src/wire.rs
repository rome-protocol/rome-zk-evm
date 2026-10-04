//! The guest's wire contract, **v3**, field-for-field
//! identical to `guest-rome::input::{RomePublicInput, RomeWitnessInput}`
//! (`rome-protocol/zisk-eth-client`, branch `deposits-guest`,
//! `crates/clients/rome/guest/src/input.rs`) — duplicated here rather than depended on across repos.
//!
//! **v3 appends the deposit range after `blocks`:** `settlement_program`, `deposit_from`,
//! `deposit_hash_from` and `deposits` (a list of [`DepositInput`]). bincode is positional, so a v3
//! public frame is exactly the v2 frame followed by those four fields; the witness frame is unchanged.
//! **Flag day:** a v2 guest ELF cannot read a v3 input and a v3 ELF cannot read a v2 one, so the
//! prover feeds only the ELF generation this crate's wire version names.
//!
//! **v2 drops `chain_config`:** v1 carried a host-supplied `chain_config`, which let a
//! prover claim a different chain's rules for the same batch — a stateless validator proves
//! consensus-validity, not derivation-canonicality. The chain's rules are now baked into the guest ELF at
//! compile time instead (`guest-rome::chain_config`); this crate stops sending `chain_config` on the wire
//! to match.
//!
//! **Why a copy, not a dependency on the fork:** this crate is built by rome-zk's own CI, which checks out
//! only the rome-zk repo — a git dependency on `rome-zk-guest` would need
//! credentials that CI does not have today (untested, not silently assumed). A path dependency into a
//! `.fork/` checkout (the shape `guest-rome` itself uses in the other direction, pointing at THIS repo)
//! only resolves in a dev worktree that happens to have the fork cloned alongside it — not in a fresh CI
//! checkout. So the wire types are copied, pinned to the exact same alloy/reth crate versions the fork
//! uses (see this crate's `Cargo.toml` comment) — bincode compatibility depends on the two sides producing
//! byte-identical encodings of identical Rust types, which needs identical crate versions, not merely "the
//! same shape". The cross-repo round-trip test this split needs (encode here, decode with the fork's own
//! types) lives at `crates/rome-zk-prover-input-cross-repo-wire` — not a gap any more.

use alloy_consensus::Header;
use alloy_rlp::{Decodable, Encodable};
use reth_ethereum_primitives::Block;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{serde_as, DeserializeAs, SerializeAs};

pub struct HeaderRlp;

impl SerializeAs<Header> for HeaderRlp {
    fn serialize_as<S: Serializer>(source: &Header, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = Vec::with_capacity(source.length());
        source.encode(&mut buf);
        buf.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Header> for HeaderRlp {
    fn deserialize_as<D: Deserializer<'de>>(deserializer: D) -> Result<Header, D::Error> {
        let buf = Vec::<u8>::deserialize(deserializer)?;
        Header::decode(&mut buf.as_slice()).map_err(serde::de::Error::custom)
    }
}

pub struct BlockRlp;

impl SerializeAs<Block> for BlockRlp {
    fn serialize_as<S: Serializer>(source: &Block, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = Vec::with_capacity(source.length());
        source.encode(&mut buf);
        buf.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Block> for BlockRlp {
    fn deserialize_as<D: Deserializer<'de>>(deserializer: D) -> Result<Block, D::Error> {
        let buf = Vec::<u8>::deserialize(deserializer)?;
        Block::decode(&mut buf.as_slice()).map_err(serde::de::Error::custom)
    }
}

/// One deposit of the batch's range, as the settlement program's queue stores it. Field-for-field
/// identical to `guest_rome::input::DepositInput`. Its index is not a field: it is the range's
/// `deposit_from` plus its position in [`RomePublicInput::deposits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepositInput {
    /// The depositor's signing public key.
    pub sender: [u8; 32],
    /// The L2 address that receives the funds.
    pub recipient: [u8; 20],
    /// The amount, in gwei.
    pub amount_gwei: u64,
}

/// Field-for-field identical to `guest_rome::RomePublicInput` (wire v3 — see this module's doc).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RomePublicInput {
    pub chain_id: u64,
    pub batch: u64,
    pub open_slot: u64,
    pub open_unix_ts: i64,
    pub max_drift_secs: u64,
    pub expected_count: u32,
    pub chunk_bodies: Vec<Vec<u8>>,
    #[serde_as(as = "HeaderRlp")]
    pub parent_header: Header,
    #[serde_as(as = "Vec<BlockRlp>")]
    pub blocks: Vec<Block>,
    /// The settlement program whose deposit queue this batch's range belongs to (wire v3).
    pub settlement_program: [u8; 32],
    /// The index of the first deposit of the batch's range `[deposit_from, deposit_from + deposits.len())`.
    pub deposit_from: u64,
    /// The queue's hash-chain value before deposit `deposit_from`.
    pub deposit_hash_from: [u8; 32],
    /// The range's deposit records, in queue order. Empty is the deposit-free batch.
    pub deposits: Vec<DepositInput>,
}

impl RomePublicInput {
    pub fn serialize(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .expect("RomePublicInput bincode encode cannot fail")
    }
}

/// Field-for-field identical to `guest_rome::RomeWitnessInput`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RomeWitnessInput {
    pub witnesses: Vec<alloy_rpc_types_debug::ExecutionWitness>,
}

impl RomeWitnessInput {
    pub fn serialize(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .expect("RomeWitnessInput bincode encode cannot fail")
    }
}

/// The two-frame stdin framing `ziskos::ZiskStdin::write_slice` produces (`guest-reth`'s own build_stdin,
/// `guest-rome`'s README): an 8-byte little-endian payload length, the payload, then zero-padding to an
/// 8-byte boundary. Implemented locally (this crate does not depend on `zisk-sdk`) so the fixture `.bin`
/// this crate writes is byte-identical to what `ZiskStdin::write_slice` would produce.
pub fn write_slice_frame(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(payload);
    let pad = (8 - (payload.len() % 8)) % 8;
    out.extend(std::iter::repeat_n(0u8, pad));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_slice_frame`'s framing must be exactly what `first_frame` (the fork's own
    /// `input-core::first_frame`, `bin/host/src/hints.rs`'s doc) expects: an 8-byte LE length prefix then
    /// the payload — this is the load-bearing byte contract between this crate and the guest's input.
    #[test]
    fn write_slice_frame_matches_the_stdin_length_prefix_contract() {
        let mut out = Vec::new();
        write_slice_frame(&mut out, &[1, 2, 3]);
        assert_eq!(&out[0..8], &3u64.to_le_bytes());
        assert_eq!(&out[8..11], &[1, 2, 3]);
        // Padded to an 8-byte boundary: 8 (len prefix) + 3 (payload) + 5 (pad) = 16.
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn write_slice_frame_needs_no_padding_when_already_aligned() {
        let mut out = Vec::new();
        write_slice_frame(&mut out, &[0u8; 8]);
        assert_eq!(out.len(), 16); // 8 (len) + 8 (payload), no pad needed
    }

    fn sample_header(number: u64) -> Header {
        Header {
            number,
            gas_used: 21_000,
            timestamp: 1_757_000_000 + number,
            ..Default::default()
        }
    }

    /// `RomePublicInput` encodes without error and its bincode bytes decode back with the
    /// standard bincode/serde stack — the same round trip `input.rs`'s test in the fork pins on that side.
    #[test]
    fn public_input_serializes_and_round_trips() {
        let input = RomePublicInput {
            chain_id: 200101,
            batch: 2043,
            open_slot: 96134,
            open_unix_ts: 1_789_337_436,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![1, 2, 3]],
            parent_header: sample_header(10),
            blocks: vec![],
            settlement_program: [0x33; 32],
            deposit_from: 0,
            deposit_hash_from: [0; 32],
            deposits: vec![],
        };
        let bytes = input.serialize();
        let (back, _): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(back.chain_id, input.chain_id);
        assert_eq!(back.parent_header, input.parent_header);
        assert_eq!(back.chunk_bodies, input.chunk_bodies);
    }

    /// Not a correctness test of this crate — writes a real, `ziskos`-stdin-framed fixture with a
    /// negative `open_unix_ts` (the guest's very first check, `RomeGuestError::NegativeOpenTs`) so it can
    /// be fed to the real `zec-rome` ELF under `ziskemu` (the "hand the guest a
    /// malformed/wrong input → refuses" check, run on real ZisK hardware rather than only host-tested).
    /// `#[ignore]`d: it writes to a fixed path for a manual follow-up run, not part of the normal suite.
    #[test]
    #[ignore]
    fn write_negative_open_ts_fixture_for_a_real_ziskemu_mutation_run() {
        let public = RomePublicInput {
            chain_id: 200101,
            batch: 1,
            open_slot: 1,
            open_unix_ts: -1,
            max_drift_secs: 60,
            expected_count: 0,
            chunk_bodies: vec![],
            parent_header: sample_header(0),
            blocks: vec![],
            settlement_program: [0x33; 32],
            deposit_from: 0,
            deposit_hash_from: [0; 32],
            deposits: vec![],
        };
        let witness = RomeWitnessInput { witnesses: vec![] };
        let mut out = Vec::new();
        write_slice_frame(&mut out, &public.serialize());
        write_slice_frame(&mut out, &witness.serialize());
        std::fs::write("/tmp/rome-guest-negative-ts.bin", out).unwrap();
    }

    /// Same rationale as the fixture above, for `RomeGuestError::MaxDriftSecsZero` (the guest's second
    /// check).
    #[test]
    #[ignore]
    fn write_zero_drift_fixture_for_a_real_ziskemu_mutation_run() {
        let public = RomePublicInput {
            chain_id: 200101,
            batch: 1,
            open_slot: 1,
            open_unix_ts: 1,
            max_drift_secs: 0,
            expected_count: 0,
            chunk_bodies: vec![],
            parent_header: sample_header(0),
            blocks: vec![],
            settlement_program: [0x33; 32],
            deposit_from: 0,
            deposit_hash_from: [0; 32],
            deposits: vec![],
        };
        let witness = RomeWitnessInput { witnesses: vec![] };
        let mut out = Vec::new();
        write_slice_frame(&mut out, &public.serialize());
        write_slice_frame(&mut out, &witness.serialize());
        std::fs::write("/tmp/rome-guest-zero-drift.bin", out).unwrap();
    }

    /// Same rationale, for a corrupted chunk body: `expected_count` claims one chunk but a malformed
    /// frame is given, so the guest's DA/channel decode must panic by name. Needs one (otherwise
    /// arbitrary) block + witness so the guest gets past its shape checks (1)-(5) and reaches the channel
    /// decode (6) — those earlier checks never inspect a block's actual contents.
    #[test]
    #[ignore]
    fn write_bad_chunk_body_fixture_for_a_real_ziskemu_mutation_run() {
        let block = reth_ethereum_primitives::Block {
            header: alloy_consensus::Header {
                number: 1,
                ..Default::default()
            },
            body: Default::default(),
        };
        let public = RomePublicInput {
            chain_id: 200101,
            batch: 1,
            open_slot: 1,
            open_unix_ts: 1,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![0u8; 5]], // too short to be a valid 19-byte frame header
            parent_header: sample_header(0),
            blocks: vec![block],
            settlement_program: [0x33; 32],
            deposit_from: 0,
            deposit_hash_from: [0; 32],
            deposits: vec![],
        };
        let witness = RomeWitnessInput {
            witnesses: vec![alloy_rpc_types_debug::ExecutionWitness::default()],
        };
        let mut out = Vec::new();
        write_slice_frame(&mut out, &public.serialize());
        write_slice_frame(&mut out, &witness.serialize());
        std::fs::write("/tmp/rome-guest-bad-chunk.bin", out).unwrap();
    }

    /// The wire v2 public frame, kept only here (this crate produces and accepts v3 only) so the one-time
    /// fixture migration can read the old bytes and the standing test below can re-derive them.
    /// bincode is positional: v3 is exactly this layout followed by the four deposit fields.
    #[serde_as]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct RomePublicInputV2 {
        chain_id: u64,
        batch: u64,
        open_slot: u64,
        open_unix_ts: i64,
        max_drift_secs: u64,
        expected_count: u32,
        chunk_bodies: Vec<Vec<u8>>,
        #[serde_as(as = "HeaderRlp")]
        parent_header: Header,
        #[serde_as(as = "Vec<BlockRlp>")]
        blocks: Vec<Block>,
    }

    impl RomePublicInputV2 {
        fn serialize(&self) -> Vec<u8> {
            bincode::serde::encode_to_vec(self, bincode::config::standard()).unwrap()
        }
    }

    impl From<&RomePublicInput> for RomePublicInputV2 {
        fn from(v3: &RomePublicInput) -> Self {
            Self {
                chain_id: v3.chain_id,
                batch: v3.batch,
                open_slot: v3.open_slot,
                open_unix_ts: v3.open_unix_ts,
                max_drift_secs: v3.max_drift_secs,
                expected_count: v3.expected_count,
                chunk_bodies: v3.chunk_bodies.clone(),
                parent_header: v3.parent_header.clone(),
                blocks: v3.blocks.clone(),
            }
        }
    }

    /// What the two committed fixtures looked like under wire v2, recorded before the migration: the
    /// sha256 and length of the public frame's payload and of the witness frame (length prefix and padding
    /// included, copied verbatim by the migration), and the facts the sidecar `.json` also states.
    struct V2Pin {
        name: &'static str,
        public_len: usize,
        public_sha256: &'static str,
        witness_frame_len: usize,
        witness_frame_sha256: &'static str,
        batch: u64,
        blocks: usize,
    }

    const V2_PINS: [V2Pin; 2] = [
        V2Pin {
            name: "txv1-dev-batch-3930",
            public_len: 6897,
            public_sha256: "cc3eac8ccd40595f85654fb82116039d7f5719e5872197bcb4c513ff0ede87b8",
            witness_frame_len: 7392,
            witness_frame_sha256:
                "1c756fe4fef84fb7c66b942bed61784914c0571ccb027d5f0de00226f75149be",
            batch: 3930,
            blocks: 10,
        },
        V2Pin {
            name: "txv1-dev-reset6-batch-1",
            public_len: 37944,
            public_sha256: "a11684585349c0a3b3449bfeb2928b23880de85fda9cf22ea01d39f2fdfbfaa9",
            witness_frame_len: 44376,
            witness_frame_sha256:
                "374e5405d3583491918820f2a851c31842ad796d338e236da822f39b5bcef994",
            batch: 1,
            blocks: 60,
        },
    ];

    /// Tiber's own `zk-settlement` program id, the program both fixtures' batches were
    /// settled under. The v2 files never recorded a settlement program; the deposit-free v3 range needs one
    /// and the chain id, and nothing else in these inputs depends on it.
    const FIXTURE_SETTLEMENT_PROGRAM: &str = "6yWj1Az1JmHBmt1654bFx2UdPMWd6Aak2QqBDPQxpj56";
    /// `h_0(FIXTURE_SETTLEMENT_PROGRAM, 200101)`, pinned as a literal so the standing test does not only
    /// compare the layouts function against itself.
    const FIXTURE_H0_HEX: &str = "5c2e7685787a6066c850bddd95ea94fef5e945180c4f1fcc6b44c162e14a7cde";

    fn fixture_path(name: &str, ext: &str) -> String {
        format!(
            "{}/../../fixtures/prover-input/{name}.{ext}",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    /// Splits a stdin file into the public frame's payload and the witness frame's raw bytes.
    fn split_frames(raw: &[u8]) -> (&[u8], &[u8]) {
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_frame_end = 8 + public_len + ((8 - (public_len % 8)) % 8);
        (&raw[8..8 + public_len], &raw[public_frame_end..])
    }

    fn fixture_settlement_program() -> [u8; 32] {
        use std::str::FromStr;
        solana_program::pubkey::Pubkey::from_str(FIXTURE_SETTLEMENT_PROGRAM)
            .unwrap()
            .to_bytes()
    }

    fn fixture_h0() -> [u8; 32] {
        let keccak = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
        rome_zk_layouts::deposit::queue_seed_hash(&keccak, &fixture_settlement_program(), 200101)
    }

    /// One-time migration of the two committed fixtures from wire v2 to wire v3, in place (it replaces the
    /// earlier v1 to v2 migration test, whose input shape no longer exists). It needs no live tunnel: every v2 field is copied untouched and the witness
    /// frame is copied byte for byte, so only the deposit-free range is new (the fixture settlement
    /// program, `deposit_from` 0, `deposit_hash_from` = `h_0`, no deposits). It refuses a file that is not
    /// the recorded v2 original, then checks its own output with the same function the standing test
    /// uses. `#[ignore]`d: it rewrites committed files, so it is run by hand, once.
    #[test]
    #[ignore]
    fn migrate_the_committed_fixtures_from_wire_v2_to_v3() {
        for pin in &V2_PINS {
            let path = fixture_path(pin.name, "bin");
            let raw = std::fs::read(&path).expect("read the committed v2 fixture");
            let (public_bytes, witness_frame) = split_frames(&raw);
            assert_eq!(
                sha256_hex(public_bytes),
                pin.public_sha256,
                "{}: not the recorded v2 original (already migrated?)",
                pin.name
            );
            let (v2, used): (RomePublicInputV2, usize) =
                bincode::serde::decode_from_slice(public_bytes, bincode::config::standard())
                    .expect("decode the v2 public frame");
            assert_eq!(used, public_bytes.len(), "{}: trailing bytes", pin.name);

            let v3 = RomePublicInput {
                chain_id: v2.chain_id,
                batch: v2.batch,
                open_slot: v2.open_slot,
                open_unix_ts: v2.open_unix_ts,
                max_drift_secs: v2.max_drift_secs,
                expected_count: v2.expected_count,
                chunk_bodies: v2.chunk_bodies,
                parent_header: v2.parent_header,
                blocks: v2.blocks,
                settlement_program: fixture_settlement_program(),
                deposit_from: 0,
                deposit_hash_from: fixture_h0(),
                deposits: vec![],
            };
            let mut out = Vec::new();
            write_slice_frame(&mut out, &v3.serialize());
            out.extend_from_slice(witness_frame);
            std::fs::write(&path, &out).expect("write the migrated v3 fixture in place");
            assert_fixture_is_v3_with_every_v2_field(pin);
        }
    }

    fn assert_fixture_is_v3_with_every_v2_field(pin: &V2Pin) {
        let raw = std::fs::read(fixture_path(pin.name, "bin")).unwrap();
        let (public_bytes, witness_frame) = split_frames(&raw);

        // The witness frame is the v2 one, byte for byte.
        assert_eq!(witness_frame.len(), pin.witness_frame_len, "{}", pin.name);
        assert_eq!(
            sha256_hex(witness_frame),
            pin.witness_frame_sha256,
            "{}: witness frame changed",
            pin.name
        );

        let (v3, used): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard())
                .expect("the fixture must decode as v3");
        assert_eq!(used, public_bytes.len(), "{}: trailing bytes", pin.name);

        // Every v2 field is unchanged: re-encoding the v3 input's v2 fields reproduces the recorded v2
        // public frame (same length, same sha256), and that frame is a strict prefix of the v3 one.
        let v2_bytes = RomePublicInputV2::from(&v3).serialize();
        assert_eq!(v2_bytes.len(), pin.public_len, "{}", pin.name);
        assert_eq!(
            sha256_hex(&v2_bytes),
            pin.public_sha256,
            "{}: a v2 field changed",
            pin.name
        );
        assert!(public_bytes.starts_with(&v2_bytes), "{}", pin.name);

        // What follows the v2 fields is exactly the deposit-free range: settlement program, deposit_from
        // 0 (one varint byte), h_0, an empty deposit list (one varint byte).
        let h0 = fixture_h0();
        let mut tail = Vec::new();
        tail.extend_from_slice(&fixture_settlement_program());
        tail.push(0);
        tail.extend_from_slice(&h0);
        tail.push(0);
        assert_eq!(
            &public_bytes[v2_bytes.len()..],
            tail.as_slice(),
            "{}",
            pin.name
        );
        assert_eq!(v3.settlement_program, fixture_settlement_program());
        assert_eq!(v3.deposit_from, 0);
        assert_eq!(v3.deposit_hash_from, h0);
        assert!(v3.deposits.is_empty());
        assert_eq!(hex::encode(h0), FIXTURE_H0_HEX, "h_0 literal");

        // And the v2 fields still say what the sidecar says about the batch.
        let sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture_path(pin.name, "json")).unwrap())
                .unwrap();
        assert_eq!(v3.chain_id, sidecar["chain_id"].as_u64().unwrap());
        assert_eq!(v3.batch, pin.batch);
        assert_eq!(
            v3.open_unix_ts as u64,
            sidecar["open_unix_ts"].as_u64().unwrap()
        );
        assert_eq!(
            v3.max_drift_secs,
            sidecar["max_drift_secs"].as_u64().unwrap()
        );
        assert_eq!(v3.blocks.len(), pin.blocks);
        assert_eq!(
            v3.blocks.first().unwrap().header.number,
            sidecar["first_number"].as_u64().unwrap()
        );
        assert_eq!(
            v3.blocks.last().unwrap().header.number,
            sidecar["last_number"].as_u64().unwrap()
        );
        assert_eq!(
            hex::encode(crate::header_hash(&v3.parent_header)),
            sidecar["parent_hash"].as_str().unwrap()
        );
    }

    /// The committed fixtures are wire v3 and carry every v2 field unchanged (the explicit migration
    /// test above wrote them; this one keeps proving it).
    #[test]
    fn committed_fixtures_are_wire_v3_with_every_v2_field_unchanged() {
        for pin in &V2_PINS {
            assert_fixture_is_v3_with_every_v2_field(pin);
        }
    }

    /// A deposit-bearing v3 input round-trips, including the record order and widths.
    #[test]
    fn public_input_with_deposits_round_trips() {
        let mut input = RomePublicInput {
            chain_id: 200101,
            batch: 7,
            open_slot: 1,
            open_unix_ts: 1_789_337_436,
            max_drift_secs: 60,
            expected_count: 1,
            chunk_bodies: vec![vec![1]],
            parent_header: sample_header(10),
            blocks: vec![],
            settlement_program: [0x33; 32],
            deposit_from: 5,
            deposit_hash_from: [0x44; 32],
            deposits: vec![],
        };
        input.deposits = vec![
            DepositInput {
                sender: [0x11; 32],
                recipient: [0x22; 20],
                amount_gwei: 1_000_000_000,
            },
            DepositInput {
                sender: [0x55; 32],
                recipient: [0x66; 20],
                amount_gwei: 2,
            },
        ];
        let (back, used): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(&input.serialize(), bincode::config::standard())
                .unwrap();
        assert_eq!(used, input.serialize().len());
        assert_eq!(back.settlement_program, input.settlement_program);
        assert_eq!(back.deposit_from, 5);
        assert_eq!(back.deposit_hash_from, input.deposit_hash_from);
        assert_eq!(back.deposits, input.deposits);
    }

    /// Writes the seven `ziskemu` refusal fixtures (reproduced against the migrated batch-3930 input, now wire v3) —
    /// four mutations on `blocks[9]` (the last of the ten blocks, number 39190) that the guest once
    /// accepted, re-run as refusals
    /// (`gas_limit` +1, `beneficiary` = 0xdead…beef, a flipped `mix_hash`, `extra_data` = "evil"), plus
    /// `withdrawals_root`/`parent_beacon_block_root` mutations that were named but not reproduced earlier,
    /// plus `public.chain_id = 1` (now expressed as a wire-level mismatch against the guest's
    /// EMBEDDED chain id, `ChainConfigIdMismatch`, since `chain_config` no longer rides on the wire at
    /// all, so a different chain config is no longer expressible). `#[ignore]`d: writes fixed paths for a
    /// manual `ziskemu` follow-up run, not part of the normal suite (same convention as this module's
    /// other fixture-writing tests).
    #[test]
    #[ignore]
    fn write_the_seven_header_rule_mutation_fixtures_for_real_ziskemu_runs() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-batch-3930.bin"
        );
        let raw = std::fs::read(path).expect("read the migrated v3 fixture");
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_bytes = &raw[8..8 + public_len];
        let public_frame_end = 8 + public_len + ((8 - (public_len % 8)) % 8);
        let witness_frame_bytes = &raw[public_frame_end..];

        let (honest, _): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard())
                .expect("decode the v3 public frame");
        let last = honest.blocks.len() - 1; // block index 9, number 39190

        let write = |name: &str, public: &RomePublicInput| {
            let mut out = Vec::new();
            write_slice_frame(&mut out, &public.serialize());
            out.extend_from_slice(witness_frame_bytes);
            std::fs::write(format!("/tmp/rome-mutation-{name}.bin"), out)
                .unwrap_or_else(|e| panic!("write /tmp/rome-mutation-{name}.bin: {e}"));
        };

        let mut m = honest.clone();
        m.blocks[last].header.gas_limit += 1;
        write("gas-limit", &m);

        let mut m = honest.clone();
        m.blocks[last].header.beneficiary = "0xdeaddeaddeaddeaddeaddeaddeaddeaddeadbeef"
            .parse()
            .unwrap();
        write("beneficiary", &m);

        let mut m = honest.clone();
        let mut mh = m.blocks[last].header.mix_hash.0;
        mh[0] ^= 0xff;
        m.blocks[last].header.mix_hash = mh.into();
        write("mix-hash", &m);

        let mut m = honest.clone();
        m.blocks[last].header.extra_data = alloy_primitives::Bytes::from_static(b"evil");
        write("extra-data", &m);

        let mut m = honest.clone();
        let mut wr = m.blocks[last]
            .header
            .withdrawals_root
            .expect("honest fixture must already carry a withdrawals_root")
            .0;
        wr[0] ^= 0xff;
        m.blocks[last].header.withdrawals_root = Some(wr.into());
        write("withdrawals-root", &m);

        let mut m = honest.clone();
        let mut pb = m.blocks[last]
            .header
            .parent_beacon_block_root
            .expect("honest fixture must already carry a parent_beacon_block_root")
            .0;
        pb[0] ^= 0xff;
        m.blocks[last].header.parent_beacon_block_root = Some(pb.into());
        write("parent-beacon-block-root", &m);

        let mut m = honest.clone();
        m.chain_id = 1;
        write("chain-id", &m);
    }
}
