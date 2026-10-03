//! Deposit-queue hash formulas: the queue's hash chain, the per-batch deposits commitment, and the
//! two-lane forced root. Every function is pure and generic over [`HashV`], so the on-chain program,
//! the off-chain services and the zkVM guest all compute the same bytes. Every `_le` integer is a
//! little-endian `u64`.
//!
//! ```text
//! h_0                  = keccak("rome-zk/deposit-queue/v1" ‖ settlement_program ‖ chain_id_le)
//! leaf_i               = keccak("rome-zk/deposit/v1" ‖ settlement_program ‖ chain_id_le ‖ i_le
//!                                ‖ sender ‖ recipient ‖ amount_gwei_le)
//! h_{i+1}              = keccak(h_i ‖ leaf_i)
//! deposits_commitment  = keccak("rome-zk/forced/deposits/v1" ‖ a_le ‖ b_le ‖ h_a ‖ h_b)
//! forced_tx_commitment = keccak("rome-zk/forced/txs/empty/v1")
//! forced_root          = forced_empty_root                      if a == b
//!                      = keccak("rome-zk/forced/v2" ‖ deposits_commitment ‖ forced_tx_commitment)
//! ```
//!
//! The preimages are 64, 126, 64, 106 and 81 bytes long. `settlement_program` and `sender` are
//! 32-byte public keys, `recipient` is a 20-byte address.

use crate::{forced_empty_root, HashV};

/// Domain of the queue's seed hash `h_0`.
pub const DEPOSIT_QUEUE_DOMAIN: &[u8] = b"rome-zk/deposit-queue/v1";
/// Domain of one deposit's leaf.
pub const DEPOSIT_LEAF_DOMAIN: &[u8] = b"rome-zk/deposit/v1";
/// Domain of the commitment to a batch's deposit range.
pub const FORCED_DEPOSITS_DOMAIN: &[u8] = b"rome-zk/forced/deposits/v1";
/// Domain of the commitment to an empty forced-transaction lane.
pub const FORCED_TXS_EMPTY_DOMAIN: &[u8] = b"rome-zk/forced/txs/empty/v1";
/// Domain of the two-lane forced root.
pub const FORCED_V2_DOMAIN: &[u8] = b"rome-zk/forced/v2";

/// One deposit as the queue stores it. Its index is not part of the record: it is the record's
/// position in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositRecord {
    /// The depositor's signing public key.
    pub sender: [u8; 32],
    /// The L2 address that receives the funds.
    pub recipient: [u8; 20],
    /// The amount, in gwei.
    pub amount_gwei: u64,
}

/// `h_0`, the hash chain's value before any deposit (64-byte preimage).
pub fn queue_seed_hash(h: &impl HashV, settlement_program: &[u8; 32], chain_id: u64) -> [u8; 32] {
    h.hashv(&[
        DEPOSIT_QUEUE_DOMAIN,
        settlement_program,
        &chain_id.to_le_bytes(),
    ])
}

/// The leaf of deposit `index` (126-byte preimage).
pub fn leaf(
    h: &impl HashV,
    settlement_program: &[u8; 32],
    chain_id: u64,
    index: u64,
    sender: &[u8; 32],
    recipient: &[u8; 20],
    amount_gwei: u64,
) -> [u8; 32] {
    h.hashv(&[
        DEPOSIT_LEAF_DOMAIN,
        settlement_program,
        &chain_id.to_le_bytes(),
        &index.to_le_bytes(),
        sender,
        recipient,
        &amount_gwei.to_le_bytes(),
    ])
}

/// One step of the hash chain: `h_{i+1} = keccak(h_i ‖ leaf_i)` (64-byte preimage).
pub fn chain_next(h: &impl HashV, prev: &[u8; 32], leaf: &[u8; 32]) -> [u8; 32] {
    h.hashv(&[prev, leaf])
}

/// Extends the chain from `h_from` (the value after deposit `from - 1`) over `records`, whose
/// indices are `from`, `from + 1`, and so on. Returns `h_to`, with `to = from + records.len()`.
/// No records returns `h_from`.
pub fn chain_through(
    h: &impl HashV,
    settlement_program: &[u8; 32],
    chain_id: u64,
    from: u64,
    h_from: &[u8; 32],
    records: &[DepositRecord],
) -> [u8; 32] {
    let mut acc = *h_from;
    for (k, r) in records.iter().enumerate() {
        let l = leaf(
            h,
            settlement_program,
            chain_id,
            from + k as u64,
            &r.sender,
            &r.recipient,
            r.amount_gwei,
        );
        acc = chain_next(h, &acc, &l);
    }
    acc
}

/// The commitment to a batch's deposit range `[a, b)`, where `h_a` and `h_b` are the chain values
/// before deposit `a` and before deposit `b` (106-byte preimage).
pub fn deposits_commitment(
    h: &impl HashV,
    a: u64,
    b: u64,
    h_a: &[u8; 32],
    h_b: &[u8; 32],
) -> [u8; 32] {
    h.hashv(&[
        FORCED_DEPOSITS_DOMAIN,
        &a.to_le_bytes(),
        &b.to_le_bytes(),
        h_a,
        h_b,
    ])
}

/// The commitment to an empty forced-transaction lane.
pub fn forced_tx_empty_commitment(h: &impl HashV) -> [u8; 32] {
    h.hashv(&[FORCED_TXS_EMPTY_DOMAIN])
}

/// The forced root of a batch whose deposit range is `[a, b)` and whose forced-transaction lane is
/// empty. An empty range gives [`forced_empty_root`], whatever `h_a` and `h_b` are, so a batch
/// without deposits keeps the value it has always had. Otherwise it is
/// `keccak("rome-zk/forced/v2" ‖ deposits_commitment ‖ forced_tx_commitment)` (81-byte preimage).
pub fn forced_root(h: &impl HashV, a: u64, b: u64, h_a: &[u8; 32], h_b: &[u8; 32]) -> [u8; 32] {
    if a == b {
        return forced_empty_root(h);
    }
    h.hashv(&[
        FORCED_V2_DOMAIN,
        &deposits_commitment(h, a, b, h_a, h_b),
        &forced_tx_empty_commitment(h),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Software keccak that records the length of every preimage it hashes, so a test can assert
    /// the exact byte length each formula feeds the hash.
    struct Recording {
        lens: RefCell<Vec<usize>>,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                lens: RefCell::new(Vec::new()),
            }
        }
        fn lens(&self) -> Vec<usize> {
            self.lens.borrow().clone()
        }
    }

    impl HashV for Recording {
        fn hashv(&self, parts: &[&[u8]]) -> [u8; 32] {
            self.lens
                .borrow_mut()
                .push(parts.iter().map(|p| p.len()).sum());
            rome_zk_merkle::keccak256(parts)
        }
    }

    fn sw() -> impl HashV {
        rome_zk_merkle::keccak256 as fn(&[&[u8]]) -> [u8; 32]
    }

    const SP: [u8; 32] = [0x33; 32];
    const CHAIN_ID: u64 = 7;

    fn records() -> [DepositRecord; 3] {
        [
            DepositRecord {
                sender: [0x11; 32],
                recipient: [0x22; 20],
                amount_gwei: 1_000_000_000,
            },
            DepositRecord {
                sender: [0x44; 32],
                recipient: [0x55; 20],
                amount_gwei: 2,
            },
            DepositRecord {
                sender: [0x66; 32],
                recipient: [0x77; 20],
                amount_gwei: u64::MAX,
            },
        ]
    }

    // The golden values below come from this script (Python, pycryptodome keccak), not from this
    // crate:
    //
    //     from Crypto.Hash import keccak
    //     import struct
    //     def kb(*p): return keccak.new(digest_bits=256, data=b"".join(p)).digest()
    //     u64 = lambda n: struct.pack("<Q", n)
    //     sp, cid = bytes([0x33]) * 32, 7
    //     recs = [(bytes([0x11])*32, bytes([0x22])*20, 1_000_000_000),
    //             (bytes([0x44])*32, bytes([0x55])*20, 2),
    //             (bytes([0x66])*32, bytes([0x77])*20, 2**64 - 1)]
    //     leaf = lambda i, r: kb(b"rome-zk/deposit/v1", sp, u64(cid), u64(i), r[0], r[1], u64(r[2]))
    //     seed = kb(b"rome-zk/deposit-queue/v1", sp, u64(cid))
    //     after1 = kb(seed, leaf(0, recs[0])); after2 = kb(after1, leaf(1, recs[1])); after3 = kb(after2, leaf(2, recs[2]))
    //     dc = kb(b"rome-zk/forced/deposits/v1", u64(0), u64(3), seed, after3)
    //     tx = kb(b"rome-zk/forced/txs/empty/v1")
    //     root = kb(b"rome-zk/forced/v2", dc, tx)
    const CHAIN_SEED: &str = "7b62297f72fe3a90eecade8f81e0197b8fef15f5b4c5a10930e1fd3bffd777f3";
    const LEAF0: &str = "152c2e9842903abc805f2922f976f9b386ec990c98d23489aec2660411c14c8c";
    const LEAF1: &str = "ab9c9f51c57f97e0136c46981845874a74be26ecd7a839e54b7797ab02d9ddb7";
    const LEAF2: &str = "7e426ccd66e46c5f197a3852fd66ab97f290d13b14b0adf90274774c31bdcbd7";
    const CHAIN_AFTER_1: &str = "8c6b4a978661d6d251ed888e8f89a4cf6b18dfdfe9cd1d1a472d6196273cdf86";
    const CHAIN_AFTER_2: &str = "30476003a29fcf37dc20c6d0071c3eb2f8d82e36761400e6ae196d05e8a6eff2";
    const CHAIN_AFTER_3: &str = "68cd40fac4d5ed9f0cdcf6f38a56a8eaf65dd1b11cc1d23e96b70eed7a85e1b2";
    const DEPOSITS_COMMITMENT_0_3: &str =
        "80ce1cb9ddd53f2eab706d26c20dea3e00ed14bd0d9bcf229980bb9abd5a86a2";
    const DEPOSITS_COMMITMENT_1_3: &str =
        "4f07686f0f043def581399ae10b8cc7d8605afeb351d8ba04b7333a89de3e4d7";
    const FORCED_TX_COMMITMENT: &str =
        "a05d5437702ee6b45d9b6626ccc0c3bba77a7e0b192956a50031e2c84e1d1e9d";
    const FORCED_ROOT_0_3: &str =
        "34f7c42e5502c795aea99b9dc9b34a1095ea347f975609863ac1d0351b8dfee8";
    const FORCED_ROOT_1_3: &str =
        "0f101d970e8e9e8ff754ad5c5d86950026a3353ee17449c9191511cbbeb245dc";
    const FORCED_EMPTY_ROOT_HEX: &str =
        "a939698ea5a2cb2ee272e900f2f3d986294007dcb635b1846b597aec642e938c";

    fn h(hex_str: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&hex_str[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn queue_seed_hash_golden() {
        assert_eq!(hex(&queue_seed_hash(&sw(), &SP, CHAIN_ID)), CHAIN_SEED);
    }

    #[test]
    fn leaf_golden() {
        let r = records();
        for (i, want) in [LEAF0, LEAF1, LEAF2].iter().enumerate() {
            let got = leaf(
                &sw(),
                &SP,
                CHAIN_ID,
                i as u64,
                &r[i].sender,
                &r[i].recipient,
                r[i].amount_gwei,
            );
            assert_eq!(&hex(&got), want, "leaf {i}");
        }
    }

    #[test]
    fn chain_next_golden() {
        assert_eq!(
            hex(&chain_next(&sw(), &h(CHAIN_SEED), &h(LEAF0))),
            CHAIN_AFTER_1
        );
        assert_eq!(
            hex(&chain_next(&sw(), &h(CHAIN_AFTER_1), &h(LEAF1))),
            CHAIN_AFTER_2
        );
    }

    #[test]
    fn chain_through_three_records_golden() {
        assert_eq!(
            hex(&chain_through(
                &sw(),
                &SP,
                CHAIN_ID,
                0,
                &h(CHAIN_SEED),
                &records()
            )),
            CHAIN_AFTER_3
        );
    }

    #[test]
    fn chain_through_a_mid_range_continues_from_h_from() {
        // Records 1 and 2 on top of h_1: the leaf indices are `from + k`.
        assert_eq!(
            hex(&chain_through(
                &sw(),
                &SP,
                CHAIN_ID,
                1,
                &h(CHAIN_AFTER_1),
                &records()[1..]
            )),
            CHAIN_AFTER_3
        );
        assert_eq!(
            hex(&chain_through(
                &sw(),
                &SP,
                CHAIN_ID,
                2,
                &h(CHAIN_AFTER_2),
                &records()[2..]
            )),
            CHAIN_AFTER_3
        );
    }

    #[test]
    fn chain_through_nothing_returns_h_from() {
        assert_eq!(
            chain_through(&sw(), &SP, CHAIN_ID, 5, &h(CHAIN_AFTER_2), &[]),
            h(CHAIN_AFTER_2)
        );
    }

    #[test]
    fn deposits_commitment_golden() {
        assert_eq!(
            hex(&deposits_commitment(
                &sw(),
                0,
                3,
                &h(CHAIN_SEED),
                &h(CHAIN_AFTER_3)
            )),
            DEPOSITS_COMMITMENT_0_3
        );
        assert_eq!(
            hex(&deposits_commitment(
                &sw(),
                1,
                3,
                &h(CHAIN_AFTER_1),
                &h(CHAIN_AFTER_3)
            )),
            DEPOSITS_COMMITMENT_1_3
        );
    }

    #[test]
    fn forced_tx_empty_commitment_golden() {
        assert_eq!(
            hex(&forced_tx_empty_commitment(&sw())),
            FORCED_TX_COMMITMENT
        );
    }

    #[test]
    fn forced_root_v2_golden() {
        assert_eq!(
            hex(&forced_root(&sw(), 0, 3, &h(CHAIN_SEED), &h(CHAIN_AFTER_3))),
            FORCED_ROOT_0_3
        );
        assert_eq!(
            hex(&forced_root(
                &sw(),
                1,
                3,
                &h(CHAIN_AFTER_1),
                &h(CHAIN_AFTER_3)
            )),
            FORCED_ROOT_1_3
        );
    }

    #[test]
    fn forced_root_of_an_empty_range_is_todays_constant() {
        // `a == b`: the constant, whatever the two hashes are.
        for (a, ha, hb) in [
            (0u64, CHAIN_SEED, CHAIN_SEED),
            (3, CHAIN_AFTER_3, CHAIN_AFTER_3),
            (9, CHAIN_AFTER_1, CHAIN_AFTER_2),
            (0, CHAIN_SEED, CHAIN_AFTER_3),
        ] {
            let got = forced_root(&sw(), a, a, &h(ha), &h(hb));
            assert_eq!(hex(&got), FORCED_EMPTY_ROOT_HEX);
            assert_eq!(got, forced_empty_root(&sw()));
        }
    }

    #[test]
    fn forced_root_a_range_never_equals_the_empty_constant() {
        assert_ne!(
            hex(&forced_root(&sw(), 0, 3, &h(CHAIN_SEED), &h(CHAIN_AFTER_3))),
            FORCED_EMPTY_ROOT_HEX
        );
    }

    #[test]
    fn preimage_lengths() {
        let r = records();

        let rec = Recording::new();
        queue_seed_hash(&rec, &SP, CHAIN_ID);
        assert_eq!(rec.lens(), vec![64], "h_0");

        let rec = Recording::new();
        leaf(
            &rec,
            &SP,
            CHAIN_ID,
            0,
            &r[0].sender,
            &r[0].recipient,
            r[0].amount_gwei,
        );
        assert_eq!(rec.lens(), vec![126], "leaf");

        let rec = Recording::new();
        chain_next(&rec, &h(CHAIN_SEED), &h(LEAF0));
        assert_eq!(rec.lens(), vec![64], "chain step");

        let rec = Recording::new();
        deposits_commitment(&rec, 0, 3, &h(CHAIN_SEED), &h(CHAIN_AFTER_3));
        assert_eq!(rec.lens(), vec![106], "deposits_commitment");

        let rec = Recording::new();
        forced_root(&rec, 0, 3, &h(CHAIN_SEED), &h(CHAIN_AFTER_3));
        // deposits_commitment (106), forced_tx_commitment (the 27-byte domain), the v2 root (81).
        assert_eq!(rec.lens(), vec![106, 27, 81], "forced_root");

        // One record costs one leaf and one chain step.
        let rec = Recording::new();
        chain_through(&rec, &SP, CHAIN_ID, 0, &h(CHAIN_SEED), &r[..1]);
        assert_eq!(rec.lens(), vec![126, 64], "chain_through, one record");
    }

    #[test]
    fn domain_constants_are_pinned() {
        assert_eq!(DEPOSIT_QUEUE_DOMAIN, b"rome-zk/deposit-queue/v1");
        assert_eq!(DEPOSIT_LEAF_DOMAIN, b"rome-zk/deposit/v1");
        assert_eq!(FORCED_DEPOSITS_DOMAIN, b"rome-zk/forced/deposits/v1");
        assert_eq!(FORCED_TXS_EMPTY_DOMAIN, b"rome-zk/forced/txs/empty/v1");
        assert_eq!(FORCED_V2_DOMAIN, b"rome-zk/forced/v2");
    }
}
