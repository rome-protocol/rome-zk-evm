//! The guest's wire contract, **v2**, field-for-field
//! identical to `guest-rome::input::{RomePublicInput, RomeWitnessInput}`
//! (`rome-protocol/zisk-eth-client`, branch `rome-guest`, `crates/clients/rome/guest/src/input.rs`) —
//! duplicated here rather than depended on across repos.
//!
//! **v2 drops `chain_config`:** v1 carried a host-supplied `chain_config`, which let a
//! prover claim a different chain's rules for the same batch — a stateless validator proves
//! consensus-validity, not derivation-canonicality. The chain's rules are now baked into the guest ELF at
//! compile time instead (`guest-rome::chain_config`); this crate stops sending `chain_config` on the wire
//! to match.
//!
//! **Why a copy, not a dependency on the fork:** this crate is built by rome-zk's own CI, which checks out
//! only the rome-zk repo — a git dependency on the (also private) `zisk-eth-client` fork would need
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

/// Field-for-field identical to `guest_rome::RomePublicInput` (wire v2 — see this module's doc).
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
        };
        let witness = RomeWitnessInput {
            witnesses: vec![alloy_rpc_types_debug::ExecutionWitness::default()],
        };
        let mut out = Vec::new();
        write_slice_frame(&mut out, &public.serialize());
        write_slice_frame(&mut out, &witness.serialize());
        std::fs::write("/tmp/rome-guest-bad-chunk.bin", out).unwrap();
    }

    /// The committed real-batch fixture
    /// (`fixtures/prover-input/txv1-dev-batch-3930.bin`) was written under wire v1 (with `chain_config`).
    /// This migrates it to v2 in place — no live tunnel needed, since every OTHER field is untouched real
    /// batch-3930 data (chain 200101, blocks 39181..=39190) and `chain_config` is the only field
    /// removed. `#[ignore]`d: a one-time migration run against a committed fixture path, not part of the
    /// normal suite (mirrors this module's other `#[ignore]`d fixture-writing tests).
    ///
    /// The v1 shape is redefined LOCALLY here (never as this crate's own public type any more — v2 is
    /// the only shape this crate produces or accepts going forward) purely so this one-time migration can
    /// decode the old bytes; `RomeWitnessInput`'s shape never changed, so its frame is copied byte for
    /// byte, unparsed.
    #[test]
    #[ignore]
    fn migrate_the_committed_batch_3930_fixture_from_wire_v1_to_v2() {
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        #[serde_as]
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct RomePublicInputV1 {
            chain_id: u64,
            batch: u64,
            open_slot: u64,
            open_unix_ts: i64,
            max_drift_secs: u64,
            expected_count: u32,
            chunk_bodies: Vec<Vec<u8>>,
            #[serde_as(as = "HeaderRlp")]
            parent_header: Header,
            #[serde_as(as = "alloy_genesis::serde_bincode_compat::ChainConfig<'_>")]
            chain_config: alloy_genesis::ChainConfig,
            #[serde_as(as = "Vec<BlockRlp>")]
            blocks: Vec<Block>,
        }

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/prover-input/txv1-dev-batch-3930.bin"
        );
        let raw = std::fs::read(path).expect("read the committed v1 fixture");

        // Two `ZiskStdin::write_slice` frames, each an 8-byte LE length prefix then the payload then
        // padding to an 8-byte boundary (`write_slice_frame`'s own doc) — read the public frame's bytes
        // to re-encode, and copy the witness frame's raw bytes (including its own length prefix and
        // padding) verbatim since its shape never changed.
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_bytes = &raw[8..8 + public_len];
        let public_frame_end = 8 + public_len + ((8 - (public_len % 8)) % 8);
        let witness_frame_bytes = &raw[public_frame_end..];

        let (v1, _): (RomePublicInputV1, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard())
                .expect("decode the v1 public frame");
        assert_eq!(
            v1.chain_config.chain_id, v1.chain_id,
            "sanity: the v1 fixture's own chain_config agreed with chain_id before it is dropped"
        );

        let v2 = RomePublicInput {
            chain_id: v1.chain_id,
            batch: v1.batch,
            open_slot: v1.open_slot,
            open_unix_ts: v1.open_unix_ts,
            max_drift_secs: v1.max_drift_secs,
            expected_count: v1.expected_count,
            chunk_bodies: v1.chunk_bodies,
            parent_header: v1.parent_header,
            blocks: v1.blocks,
        };

        let mut out = Vec::new();
        write_slice_frame(&mut out, &v2.serialize());
        out.extend_from_slice(witness_frame_bytes);
        std::fs::write(path, &out).expect("write the migrated v2 fixture in place");

        // Re-read what was just written and confirm it round-trips as v2 before trusting the file.
        let reread = std::fs::read(path).unwrap();
        let reread_public_len = u64::from_le_bytes(reread[0..8].try_into().unwrap()) as usize;
        let (back, _): (RomePublicInput, usize) = bincode::serde::decode_from_slice(
            &reread[8..8 + reread_public_len],
            bincode::config::standard(),
        )
        .expect("the migrated fixture must decode as v2");
        assert_eq!(back.chain_id, v1.chain_id);
        assert_eq!(
            back.blocks.len(),
            10,
            "batch 3930: blocks 39181..=39190, ten blocks (all empty on idle Tiber)"
        );
    }

    /// Writes the seven `ziskemu` refusal fixtures (reproduced against the migrated v2 batch-3930 input) —
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
        let raw = std::fs::read(path).expect("read the migrated v2 fixture");
        let public_len = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
        let public_bytes = &raw[8..8 + public_len];
        let public_frame_end = 8 + public_len + ((8 - (public_len % 8)) % 8);
        let witness_frame_bytes = &raw[public_frame_end..];

        let (honest, _): (RomePublicInput, usize) =
            bincode::serde::decode_from_slice(public_bytes, bincode::config::standard())
                .expect("decode the v2 public frame");
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
