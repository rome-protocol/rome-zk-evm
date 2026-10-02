//! Measurement guest logic. **Not the production guest** — no witnessed block execution, no public-values-v2
//! commit. This measures the guest's two open unknowns before it is built: whether the channel's `ruzstd` decode is
//! a material share of a loaded batch's steps (above 20% counts as material), and the single-execution ceiling one
//! A100 can carry.
//!
//! `rome-zk-layouts` now gates `solana-program` behind a default-on `solana` feature (every `pda()` needing
//! `Pubkey`, 11 modules) so the crate builds for the ZisK guest target with `default-features = false`;
//! `rome-zk-channel` follows the same way (`default-features = false, features = ["decode-pure"]` — it never
//! touched a `Pubkey` to begin with, only `frame::*`). The bench `shim` module this crate used to carry (a
//! duplicate of the 19-byte frame header and the `acc`/ `forced_empty_root` formulas, kept byte-identical to
//! `rome-zk-layouts` by a pinning test) is gone: this crate now depends on the real crates unconditionally and
//! calls them directly — `rome_zk_layouts::{forced_empty_root, acc}`, `rome_zk_channel::{Frame::from_bytes,
//! reassemble, decompress_pure}` — one definition of each fact, in the crate that owns it.
//!
//! Three stages, matching `zk_inbox_client::reference_commitment`:
//! 1. **hash** (always run): `keccak(body_i)` per chunk → indexed leaf → merkle root → forced root → acc.
//! 2. **reassemble + ruzstd** (feature `stage_ruzstd`): frame parse, reassemble, `ruzstd` decompress (pure
//!    Rust — the ZisK target has no C toolchain).
//! 3. **RLP decode** (feature `stage_rlp`, implies `stage_ruzstd`): the decompressed bytes → block count.
//!
//! `bin/` is the ~10-line ZisK entrypoint over [`run`]; every stage above is host-testable here with no
//! `ziskos` dependency, so `cargo test` runs natively (still on a build machine per repo convention — this
//! crate's `Cargo.lock` is independent of the root workspace's).
//!
//! `run` commits `(block_count, acc)` rather than a single mixed hash: `bin/`'s entrypoint commits
//! `acc` directly, so `ziskemu -o commits.bin`'s output can be compared byte-for-byte against a real
//! fixture's on-chain `acc` (`fixtures/inbox/txv1-dev-batch-2043.json`, `965a19bc…`) — see
//! [`tests::stage_hash_reproduces_the_real_fixtures_on_chain_acc`].

/// The bench guest's public input: raw chunk-account bodies, idx order — each one exactly a
/// `rome_zk_channel::Frame::to_bytes()` encoding (19-byte frame header + compressed body slice), i.e. the
/// bytes after a chunk account's 64-byte header (`zk-inbox-client::decode_chunk_header` /
/// `rome_zk_layouts::chunk::HEADER_LEN`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BenchInput {
    pub chain_id: u64,
    pub batch: u64,
    pub open_slot: u64,
    pub expected_count: u32,
    pub chunk_bodies: Vec<Vec<u8>>,
}

/// A local, RLP-field-order mirror of `rome_zk_channel::Block` (`number, timestamp, gas_limit, txs`), used
/// only so stage 3 can RLP-decode a block LIST's length without pulling in `rome_zk_channel::Block`'s own
/// derive target — kept identical to `rome_zk_channel::Block`'s field order (channel owns the RLP shape;
/// this struct never diverges from it, proved by
/// [`tests::block_count_matches_the_real_channel_decoder`]). Never constructed with real field values
/// here, only decoded into, and only `.len()` of the outer `Vec` is read.
#[derive(Debug, Clone, PartialEq, Eq, alloy_rlp::RlpDecodable)]
struct CountOnlyBlock {
    #[allow(dead_code)]
    number: u64,
    #[allow(dead_code)]
    timestamp: u64,
    #[allow(dead_code)]
    gas_limit: u64,
    #[allow(dead_code)]
    txs: Vec<alloy_primitives::Bytes>,
}

/// The one keccak this crate uses — [`rome_zk_merkle::keccak256`], never re-implemented. Cast to a concrete `fn`
/// pointer so it can be passed wherever `&impl HashV` is expected, matching the pattern
/// `zk-inbox-client::reference_commitment` already uses.
fn hash_fn() -> fn(&[&[u8]]) -> [u8; 32] {
    rome_zk_merkle::keccak256
}

/// Stage 1 (always run): `(root, forced_root, acc)` via the real `rome-zk-merkle`/`rome-zk-layouts`
/// functions — byte-for-byte what `zk_inbox_client::reference_commitment` computes off-chain and what the
/// inbox program computes on-chain, given the same `chunk_hashes`.
pub fn stage_hash(input: &BenchInput) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let h = hash_fn();
    let chunk_hashes: Vec<[u8; 32]> = input.chunk_bodies.iter().map(|body| h(&[body])).collect();
    let leaves: Vec<[u8; 32]> = chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, hash)| rome_zk_merkle::indexed_leaf(&h, i as u32, hash))
        .collect();
    let root = rome_zk_merkle::root(&h, &leaves);
    let forced_root = rome_zk_layouts::forced_empty_root(&h);
    let acc = rome_zk_layouts::acc(
        &h,
        input.chain_id,
        input.batch,
        input.open_slot,
        input.expected_count,
        &root,
        &forced_root,
    );
    (root, forced_root, acc)
}

/// Stage 2 (feature `stage_ruzstd`): `rome_zk_channel::Frame::from_bytes` on each chunk body, `reassemble`
/// (accepts any frame order, refuses a foreign `channel_id` or a missing frame by name) then
/// `decompress_pure` (the pure-Rust `ruzstd` path — the ZisK target has no C toolchain). Returns the
/// decompressed RLP bytes.
#[cfg(feature = "stage_ruzstd")]
pub fn stage_reassemble_and_decompress(
    chunk_bodies: &[Vec<u8>],
) -> Result<Vec<u8>, rome_zk_channel::ChannelError> {
    let frames: Vec<rome_zk_channel::Frame> = chunk_bodies
        .iter()
        .map(|b| rome_zk_channel::Frame::from_bytes(b))
        .collect::<Result<_, _>>()?;
    let compressed = rome_zk_channel::reassemble(&frames)?;
    rome_zk_channel::decompress_pure(&compressed)
}

/// Stage 3 (feature `stage_rlp`, implies `stage_ruzstd`): RLP-decodes the decompressed bytes into a block
/// list — field-order-identical to `rome_zk_channel::Block` — and returns the count, all the guest's
/// downstream commit needs.
#[cfg(feature = "stage_rlp")]
pub fn stage_rlp_decode(decompressed: &[u8]) -> Result<u64, rome_zk_channel::ChannelError> {
    let mut slice = decompressed;
    let blocks = <Vec<CountOnlyBlock> as alloy_rlp::Decodable>::decode(&mut slice)
        .map_err(rome_zk_channel::ChannelError::Rlp)?;
    if !slice.is_empty() {
        return Err(rome_zk_channel::ChannelError::Rlp(
            alloy_rlp::Error::UnexpectedLength,
        ));
    }
    Ok(blocks.len() as u64)
}

/// What `bin/` actually calls: `(block_count, acc)`. Panics by name on any stage failure (what a real ZisK
/// guest does via `.expect(...)`, and what the mutation tests assert against) — `bin/`'s entrypoint commits
/// both values directly (never mixed into one hash) so `ziskemu`'s committed output carries `acc` in the
/// clear, comparable byte-for-byte against a real batch's on-chain value.
pub fn run(input: &BenchInput) -> (u64, [u8; 32]) {
    let (_root, _forced_root, acc) = stage_hash(input);

    #[cfg(feature = "stage_ruzstd")]
    let block_count: u64 = {
        let decompressed = stage_reassemble_and_decompress(&input.chunk_bodies)
            .expect("stage 2 (reassemble + ruzstd decompress) failed");
        #[cfg(feature = "stage_rlp")]
        {
            stage_rlp_decode(&decompressed).expect("stage 3 (RLP decode) failed")
        }
        #[cfg(not(feature = "stage_rlp"))]
        {
            decompressed.len() as u64
        }
    };
    #[cfg(not(feature = "stage_ruzstd"))]
    let block_count: u64 = 0;

    (block_count, acc)
}

/// A `tx_len`-byte pseudo-random-looking transaction body: mostly xorshift64 bytes (standing in for a
/// signature/hash, which real zstd cannot compress away) with the first 21 bytes fixed (standing in for a
/// legacy transfer's shared `to`/method shape, which real zstd DOES compress across a batch of transfers
/// to/from the same small set of addresses) — this is why the measured 74 B/tx (zstd-19) is well below
/// `tx_len` itself; a fully-repeated byte pattern (this generator's first draft) compressed to near
/// nothing and was not a fair proxy for a loaded batch's real decode cost.
pub fn pseudo_random_tx(block: u64, idx: u64, tx_len: usize) -> Vec<u8> {
    let mut state = 0x9E3779B97F4A7C15u64 ^ (block.wrapping_mul(1_000_003)) ^ idx;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::with_capacity(tx_len);
    // Fixed prefix: same "to" address + method selector shape on every tx (realistic — a batch of legacy
    // transfers shares its recipient set), so zstd has real cross-tx redundancy to find, same as the
    // premise's measured ratio assumes.
    let fixed_len = tx_len.min(21);
    out.extend(std::iter::repeat(0xABu8).take(fixed_len));
    while out.len() < tx_len {
        out.extend_from_slice(&next().to_le_bytes());
    }
    out.truncate(tx_len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_block(number: u64, tx_count: usize, tx_len: usize) -> rome_zk_channel::Block {
        rome_zk_channel::Block {
            number,
            timestamp: 1_757_000_000 + number,
            gas_limit: 40_000_000,
            txs: (0..tx_count)
                .map(|i| {
                    alloy_primitives::Bytes::from(vec![(number * 131 + i as u64) as u8; tx_len])
                })
                .collect(),
        }
    }

    /// Loaded-batch blocks shaped like gen_input's synthetic fixtures (pseudo-random tx bodies with a
    /// shared 21-byte prefix) — these compress to MANY frames, unlike `sample_block`'s repeated bytes
    /// (10 x 300 of those fit one 892-byte chunk).
    fn loaded_blocks(
        n_blocks: u64,
        txs_per_block: usize,
        tx_len: usize,
    ) -> Vec<rome_zk_channel::Block> {
        (0..n_blocks)
            .map(|n| rome_zk_channel::Block {
                number: n,
                timestamp: 1_757_000_000 + n,
                gas_limit: 40_000_000,
                txs: (0..txs_per_block)
                    .map(|i| alloy_primitives::Bytes::from(pseudo_random_tx(n, i as u64, tx_len)))
                    .collect(),
            })
            .collect()
    }

    /// Builds `chunk_bodies` for a batch of `blocks`, encoded and cut exactly the way the real batcher does
    /// (`rome_zk_channel::encode_stream` + `cut_frames`, the default frame size), so the synthetic
    /// fixtures match what the batcher produces.
    fn chunk_bodies_for(
        chain_id: u64,
        batch: u64,
        blocks: &[rome_zk_channel::Block],
    ) -> Vec<Vec<u8>> {
        let compressed = rome_zk_channel::encode_stream(blocks);
        let frames = rome_zk_channel::cut_frames(
            chain_id,
            batch,
            &compressed,
            rome_zk_channel::DEFAULT_MAX_FRAME_BODY_LEN,
        );
        frames.iter().map(|f| f.to_bytes()).collect()
    }

    /// Load-bearing correctness check: `stage_hash` must equal
    /// `zk_inbox_client::reference_commitment`'s formula — proven directly against
    /// `rome_zk_layouts::acc`/`rome_zk_merkle` (this lib crate is its own workspace with no path back into
    /// the root one, so `zk-inbox-client` itself cannot be a dependency here) by recomputing the same
    /// inputs independently.
    #[test]
    fn stage_hash_matches_the_accumulator_formula_for_a_known_fixture() {
        let blocks = vec![sample_block(0, 2, 10), sample_block(1, 1, 5)];
        let chunk_bodies = chunk_bodies_for(11, 22, &blocks);
        let input = BenchInput {
            chain_id: 11,
            batch: 22,
            open_slot: 5,
            expected_count: chunk_bodies.len() as u32,
            chunk_bodies: chunk_bodies.clone(),
        };
        let (root, forced_root, acc) = stage_hash(&input);

        let h = rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32];
        let chunk_hashes: Vec<[u8; 32]> = chunk_bodies.iter().map(|b| h(&[b])).collect();
        let leaves: Vec<[u8; 32]> = chunk_hashes
            .iter()
            .enumerate()
            .map(|(i, hh)| rome_zk_merkle::indexed_leaf(&h, i as u32, hh))
            .collect();
        let expected_root = rome_zk_merkle::root(&h, &leaves);
        let expected_forced = rome_zk_layouts::forced_empty_root(&h);
        let expected_acc = rome_zk_layouts::acc(
            &h,
            11,
            22,
            5,
            chunk_bodies.len() as u32,
            &expected_root,
            &expected_forced,
        );

        assert_eq!(root, expected_root);
        assert_eq!(forced_root, expected_forced);
        assert_eq!(acc, expected_acc);
    }

    /// The decode path must recover exactly the blocks the real batcher encoder produced, and
    /// must equal `rome_zk_channel::decode_stream` (the canonical C-zstd decoder every other consumer
    /// uses) — proving the guest's decode agrees byte-for-byte with what the batcher/derive already
    /// trust.
    #[cfg(all(feature = "stage_ruzstd", feature = "stage_rlp"))]
    #[test]
    fn reference_pipeline_recovers_the_real_encoder_blocks_and_matches_the_c_decoder() {
        let blocks: Vec<rome_zk_channel::Block> =
            (0..10).map(|n| sample_block(n, 300, 110)).collect();
        let chunk_bodies = chunk_bodies_for(200_101, 1, &blocks);
        let decompressed = stage_reassemble_and_decompress(&chunk_bodies).unwrap();
        let count = stage_rlp_decode(&decompressed).unwrap();
        assert_eq!(count, blocks.len() as u64);

        let compressed = rome_zk_channel::encode_stream(&blocks);
        let via_c = rome_zk_channel::decode_stream(&compressed).unwrap();
        assert_eq!(via_c, blocks);
        let via_pure = rome_zk_channel::decode_stream_pure(&compressed).unwrap();
        assert_eq!(via_pure, blocks);
    }

    /// The count-only RLP decode (stage 3, no `rome_zk_channel::Block` dependency) must agree with the
    /// real channel decoder's block count — proves [`CountOnlyBlock`] has not drifted from
    /// `rome_zk_channel::Block`'s field order.
    #[cfg(all(feature = "stage_ruzstd", feature = "stage_rlp"))]
    #[test]
    fn block_count_matches_the_real_channel_decoder() {
        let blocks: Vec<rome_zk_channel::Block> = (0..5).map(|n| sample_block(n, 20, 64)).collect();
        let chunk_bodies = chunk_bodies_for(1, 1, &blocks);
        let decompressed = stage_reassemble_and_decompress(&chunk_bodies).unwrap();
        let count = stage_rlp_decode(&decompressed).unwrap();
        let via_channel =
            rome_zk_channel::decode_stream_pure(&rome_zk_channel::encode_stream(&blocks)).unwrap();
        assert_eq!(count, via_channel.len() as u64);
    }

    /// Frames land in any order (parallel send): reassemble must commit identically for a reversed
    /// frame order. Mutation: order frames by `u16::MAX - frame_no` → red.
    #[cfg(all(feature = "stage_ruzstd", feature = "stage_rlp"))]
    #[test]
    fn reassemble_accepts_frames_in_any_order() {
        let blocks = loaded_blocks(10, 300, 110);
        let mut chunk_bodies = chunk_bodies_for(200_101, 7, &blocks);
        assert!(chunk_bodies.len() > 1);
        let in_order = chunk_bodies.clone();
        chunk_bodies.reverse();
        let reversed = chunk_bodies;
        assert_eq!(
            stage_reassemble_and_decompress(&reversed).unwrap(),
            stage_reassemble_and_decompress(&in_order).unwrap()
        );
    }

    /// A frame from ANOTHER batch (different channel_id) mixed in, or a missing frame, must be refused by
    /// name — never silently overwrite a same-numbered frame and decode garbage.
    ///
    /// **Not covered here: a duplicated frame_no.** `rome_zk_channel::reassemble` refuses a repeated
    /// `frame_no` by name (`ChannelError::DuplicateFrame`), identical body or not, and that crate's own
    /// tests pin it; this test covers only a foreign channel_id and a missing frame.
    #[cfg(all(feature = "stage_ruzstd", feature = "stage_rlp"))]
    #[test]
    fn reassemble_refuses_a_foreign_channel_id_and_a_missing_frame_by_name() {
        let blocks = loaded_blocks(10, 300, 110);
        let bodies_a = chunk_bodies_for(200_101, 7, &blocks);
        let bodies_b = chunk_bodies_for(200_101, 8, &blocks);
        assert!(bodies_a.len() > 2);

        let mut mixed = bodies_a.clone();
        mixed[1] = bodies_b[1].clone();
        match stage_reassemble_and_decompress(&mixed) {
            Err(rome_zk_channel::ChannelError::ChannelIdMismatch { .. }) => {}
            other => panic!("expected a ChannelIdMismatch refusal, got {other:?}"),
        }

        let mut missing = bodies_a.clone();
        missing.remove(1);
        match stage_reassemble_and_decompress(&missing) {
            Err(rome_zk_channel::ChannelError::MissingFrame(_)) => {}
            other => panic!("expected a MissingFrame refusal, got {other:?}"),
        }
    }

    /// Mutation: corrupting one byte of one chunk's compressed body must make `run` panic by name, not
    /// silently produce a wrong commit or a generic panic — this is exactly what a real ZisK guest
    /// execution would do on tampered DA bytes.
    #[cfg(all(feature = "stage_ruzstd", feature = "stage_rlp"))]
    #[test]
    #[should_panic(expected = "stage 2 (reassemble + ruzstd decompress) failed")]
    fn run_panics_by_name_on_a_corrupted_chunk_body() {
        let blocks: Vec<rome_zk_channel::Block> = (0..3).map(|n| sample_block(n, 50, 64)).collect();
        let mut chunk_bodies = chunk_bodies_for(1, 1, &blocks);
        let corrupt_at = rome_zk_layouts::frame::FRAME_HEADER_LEN + 2;
        chunk_bodies[0][corrupt_at] ^= 0xff;
        let input = BenchInput {
            chain_id: 1,
            batch: 1,
            open_slot: 1,
            expected_count: chunk_bodies.len() as u32,
            chunk_bodies,
        };
        let _ = run(&input);
    }

    /// `run` must be deterministic and produce a real, non-zero `acc` on a valid input.
    #[test]
    fn run_is_deterministic_on_a_valid_input() {
        let blocks = vec![sample_block(0, 1, 8)];
        let chunk_bodies = chunk_bodies_for(7, 3, &blocks);
        let input = BenchInput {
            chain_id: 7,
            batch: 3,
            open_slot: 1,
            expected_count: chunk_bodies.len() as u32,
            chunk_bodies,
        };
        let (count_a, acc_a) = run(&input);
        let (count_b, acc_b) = run(&input);
        assert_eq!((count_a, acc_a), (count_b, acc_b));
        assert_ne!(acc_a, [0u8; 32]);
    }

    /// Fixture JSON shape written by `fetch_real_batch`: hex chunk bodies plus the on-chain
    /// values recomputed at fetch time — `acc`/`root`/`forced_root` are the values this test checks
    /// `stage_hash` reproduces, not merely round-tripped input.
    #[derive(serde::Deserialize)]
    struct RealFixture {
        chain_id: u64,
        batch: u64,
        open_slot: u64,
        expected_count: u32,
        acc: String,
        chunk_bodies_hex: Vec<String>,
    }

    fn load_real_fixture() -> (BenchInput, [u8; 32]) {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../fixtures/inbox/txv1-dev-batch-2043.json"
        );
        let raw = std::fs::read_to_string(path).expect("read the committed real-batch fixture");
        let f: RealFixture = serde_json::from_str(&raw).expect("parse fixture json");
        let chunk_bodies: Vec<Vec<u8>> = f
            .chunk_bodies_hex
            .iter()
            .map(|h| hex::decode(h).expect("hex-decode chunk body"))
            .collect();
        let acc_bytes: [u8; 32] = hex::decode(&f.acc)
            .expect("hex-decode acc")
            .try_into()
            .expect("acc is 32 bytes");
        (
            BenchInput {
                chain_id: f.chain_id,
                batch: f.batch,
                open_slot: f.open_slot,
                expected_count: f.expected_count,
                chunk_bodies,
            },
            acc_bytes,
        )
    }

    /// The load-bearing fixture check: `stage_hash`'s `acc` over the committed real-batch fixture must equal the
    /// on-chain `acc` `fetch_real_batch` recorded when it read the finalized batch back from the dev chain — this
    /// is the same equality `ziskemu -m` reproduces at the ELF level (guest README); this test proves the formula
    /// on the host, independent of the ZisK runtime.
    #[test]
    fn stage_hash_reproduces_the_real_fixtures_on_chain_acc() {
        let (input, expected_acc) = load_real_fixture();
        let (_root, _forced_root, acc) = stage_hash(&input);
        assert_eq!(
            acc, expected_acc,
            "acc mismatch: recomputed acc does not match the fixture's recorded on-chain acc"
        );
    }

    /// Mutation: flipping one byte of the real fixture's chunk body must make the recomputed `acc` disagree with
    /// the fixture's recorded on-chain value — refused by name ("acc mismatch"), never silently accepted. This is
    /// the host-provable half of "ziskemu -m on the fixture reproduces the acc"; the guest itself refuses the same
    /// way because `bin/`'s entrypoint commits `acc` directly and a downstream consumer compares it against the
    /// expected on-chain value the same way this test does.
    #[test]
    #[should_panic(expected = "acc mismatch")]
    fn corrupting_the_real_fixtures_chunk_body_breaks_the_acc_match() {
        let (mut input, expected_acc) = load_real_fixture();
        let corrupt_at = input.chunk_bodies[0].len() - 1;
        input.chunk_bodies[0][corrupt_at] ^= 0xff;
        let (_root, _forced_root, acc) = stage_hash(&input);
        assert_eq!(
            acc, expected_acc,
            "acc mismatch: recomputed acc does not match the fixture's recorded on-chain acc"
        );
    }
}
