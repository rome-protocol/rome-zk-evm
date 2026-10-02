//! Bounded Ethereum Merkle-Patricia (MPT) proof verifier: [`verify_account`] proves an account's fields
//! against a proof-bound `state_root`; [`verify_storage`] proves a storage slot's value (or its absence)
//! against an account's `storage_root`. Built for exits: an exit proof is exactly this — an account proof of
//! the L2 exit portal plus a storage proof of `sentMessages[message_hash]` — but neither function above names
//! "exit" or "portal" anywhere; this crate is a general two-call Ethereum MPT verifier with no knowledge of
//! what calls it.
//!
//! ## What this crate does NOT check Everything here assumes `state_root`/`storage_root` are already the roots a
//! caller trusts — this crate only proves that `nodes` is a valid Merkle-Patricia path from that root to the
//! claimed value (or to a provable absence). The root's own provenance — that it really is a finalized batch's
//! proof-bound `state_root`, read from the right pending/root PDA for the right chain and batch — is entirely the
//! settlement program's job (`ProveExit`); this crate has no opinion on where a root came from.
//!
//! ## Bounds (checked before any hashing or decoding)
//! [`MAX_NODES`] (64) and [`MAX_NODE_BYTES`] (532, a branch node's worst case: 16 × 33-byte hash slots
//! plus a short value slot and RLP overhead) bound every proof this crate accepts — [`check_bounds`] runs
//! first in both [`verify_account`] and [`verify_storage`], before `nodes[0]`'s keccak is ever taken.
//!
//! ## Inclusion vs exclusion
//! [`verify_account`] has no exclusion outcome: every real caller (`ProveExit`) only ever asks about a
//! portal address it expects to exist, so a path that diverges from the claimed address is refused
//! outright ([`MptError::PathMismatch`]), not returned as "account absent". [`verify_storage`] is the
//! opposite — [`StorageValue::Absent`] is a **distinct, explicit** outcome, never conflated with
//! `Present([0u8; 32])`: a slot whose value happens to be zero and a slot that was never written are
//! different facts, and this crate's own type system keeps a caller from confusing them.
//!
//! ## Hashing Every hash in this crate goes through [`rome_zk_merkle::keccak256`] — the one keccak owner in the
//! workspace — never a locally-linked `sha3`/keccak dependency. That function already dispatches to the
//! `solana_program::keccak::hashv` syscall on `target_os = "solana"` and a pure Rust implementation everywhere
//! else, so this crate needs no target-specific code of its own for that.

#![forbid(unsafe_code)]

mod nibbles;
mod rlp;
mod walk;

use borsh::{BorshDeserialize, BorshSerialize};

/// A trie node's encoded byte length may not exceed this — a branch node's worst case is 16 × (1 header
/// byte + 32-byte hash) + up to 2 bytes of value slot + a handful of RLP list-header bytes; 532 covers it
/// with headroom.
pub const MAX_NODE_BYTES: usize = 532;

/// A single account or storage proof may not carry more than this many nodes — the deepest a real
/// Ethereum trie ever gets in practice is well under this; a proof claiming more is refused outright,
/// never merely truncated.
pub const MAX_NODES: usize = 64;

/// Every way this crate refuses a proof, by name — never a silent `Ok` on a claim that does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MptError {
    /// `nodes.len()` exceeded [`MAX_NODES`].
    TooManyNodes { got: usize },
    /// `nodes[index].len()` exceeded [`MAX_NODE_BYTES`].
    NodeTooLarge { index: usize, len: usize },
    /// `keccak256(nodes[0])` did not equal the root this proof was checked against.
    RootMismatch,
    /// `keccak256(nodes[index])` did not equal the hash the previous node referenced it by.
    HashMismatch { index: usize },
    /// The claimed key's nibble path disagreed with a leaf's stored path or an extension's stored prefix,
    /// in a context where that disagreement is refused rather than reported as exclusion (account proofs
    /// only — see the module doc).
    PathMismatch,
    /// `nodes[index]` (or an embedded child inside it) is not well-formed RLP, or not the shape a trie
    /// node/child-reference is allowed to take.
    BadRlp { index: usize },
    /// `nodes[index]` decoded as an RLP list, but with neither 2 (leaf/extension) nor 17 (branch) items.
    BadNodeShape { index: usize, items: usize },
    /// The proof supplied more nodes than the walk from the root to the claimed key ever consumed.
    TrailingNodes,
    /// The value bytes at the end of a walk did not decode as the shape the caller needed: for
    /// [`verify_account`], the 4-item `[nonce, balance, storage_root, code_hash]` account RLP list; for
    /// [`verify_storage`], the double-RLP-wrapped scalar (see [`decode_storage_value`]'s doc) not itself
    /// being a well-formed RLP string. Unreachable against any real `eth_getProof` output (both shapes are
    /// exactly what go-ethereum always writes) — a defensive bound, not a case any committed fixture hits.
    BadAccountRlp,
    /// A decoded integer (an account's `nonce`/`balance`, or a storage value) used more bytes than its
    /// type allows (8 for `nonce`, 32 for `balance`/a storage value).
    ValueTooLong,
}

/// The four fields an Ethereum account's trie leaf commits, decoded from its RLP value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub nonce: u64,
    /// Big-endian, left-padded to 32 bytes.
    pub balance: [u8; 32],
    pub storage_root: [u8; 32],
    pub code_hash: [u8; 32],
}

/// The outcome of a storage-slot proof: a slot that was never written is [`StorageValue::Absent`], never
/// [`StorageValue::Present`] with a zero value — the two are different facts about the trie and this
/// type keeps a caller from conflating them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageValue {
    /// Big-endian, left-padded to 32 bytes.
    Present([u8; 32]),
    Absent,
}

/// Checks `nodes` against [`MAX_NODES`]/[`MAX_NODE_BYTES`] — run before any hashing or RLP decoding, in
/// both [`verify_account`] and [`verify_storage`].
fn check_bounds(nodes: &[Vec<u8>]) -> Result<(), MptError> {
    if nodes.len() > MAX_NODES {
        return Err(MptError::TooManyNodes { got: nodes.len() });
    }
    for (index, n) in nodes.iter().enumerate() {
        if n.len() > MAX_NODE_BYTES {
            return Err(MptError::NodeTooLarge {
                index,
                len: n.len(),
            });
        }
    }
    Ok(())
}

/// Proves `address`'s account fields against `state_root`: `nodes` must be a Merkle-Patricia path from
/// `state_root` to the leaf at `keccak256(address)`. There is no exclusion outcome — see the module doc.
pub fn verify_account(
    state_root: &[u8; 32],
    address: &[u8; 20],
    nodes: &[Vec<u8>],
) -> Result<Account, MptError> {
    check_bounds(nodes)?;
    let key = rome_zk_merkle::keccak256(&[address.as_slice()]);
    let key_nibbles = nibbles::to_nibbles(&key);
    let (outcome, consumed) = walk::walk(*state_root, &key_nibbles, nodes)?;
    if consumed != nodes.len() {
        return Err(MptError::TrailingNodes);
    }
    match outcome {
        walk::WalkOutcome::Found(value) => decode_account_rlp(value),
        walk::WalkOutcome::Diverged => Err(MptError::PathMismatch),
    }
}

/// Proves (or disproves) `slot`'s value in the storage trie rooted at `storage_root`: `nodes` must be a
/// Merkle-Patricia path from `storage_root` toward the leaf at `keccak256(slot)`. Unlike
/// [`verify_account`], a path that diverges from the trie is a legitimate, distinct outcome
/// ([`StorageValue::Absent`]), not an error — an exclusion proof and a malformed proof are different
/// things, and only the caller who asked "is this slot set" can tell them apart from an inclusion proof.
pub fn verify_storage(
    storage_root: &[u8; 32],
    slot: &[u8; 32],
    nodes: &[Vec<u8>],
) -> Result<StorageValue, MptError> {
    check_bounds(nodes)?;
    let key = rome_zk_merkle::keccak256(&[slot.as_slice()]);
    let key_nibbles = nibbles::to_nibbles(&key);
    let (outcome, consumed) = walk::walk(*storage_root, &key_nibbles, nodes)?;
    if consumed != nodes.len() {
        return Err(MptError::TrailingNodes);
    }
    match outcome {
        walk::WalkOutcome::Found(value) => Ok(StorageValue::Present(decode_storage_value(value)?)),
        walk::WalkOutcome::Diverged => Ok(StorageValue::Absent),
    }
}

/// Decodes a leaf/branch value slot's bytes as the account RLP shape: a 4-item list `[nonce, balance,
/// storage_root, code_hash]`. Go-ethereum writes an account's trie value as `rlp.EncodeToBytes(account)`
/// — that already-list-shaped encoding is what a leaf's own (single) RLP-string wrap unwraps to, so this
/// decodes the list directly with no further unwrap (contrast [`decode_storage_value`], which needs one).
fn decode_account_rlp(bytes: &[u8]) -> Result<Account, MptError> {
    let items = rlp::decode_top_level_list(bytes).map_err(|_| MptError::BadAccountRlp)?;
    let [nonce_item, balance_item, root_item, code_item] = items.as_slice() else {
        return Err(MptError::BadAccountRlp);
    };
    let rlp::Item::Str(nonce_bytes) = *nonce_item else {
        return Err(MptError::BadAccountRlp);
    };
    let rlp::Item::Str(balance_bytes) = *balance_item else {
        return Err(MptError::BadAccountRlp);
    };
    let rlp::Item::Str(storage_root_bytes) = *root_item else {
        return Err(MptError::BadAccountRlp);
    };
    let rlp::Item::Str(code_hash_bytes) = *code_item else {
        return Err(MptError::BadAccountRlp);
    };

    if nonce_bytes.len() > 8 {
        return Err(MptError::ValueTooLong);
    }
    let mut nonce_buf = [0u8; 8];
    nonce_buf[8 - nonce_bytes.len()..].copy_from_slice(nonce_bytes);

    if balance_bytes.len() > 32 {
        return Err(MptError::ValueTooLong);
    }
    let mut balance = [0u8; 32];
    balance[32 - balance_bytes.len()..].copy_from_slice(balance_bytes);

    if storage_root_bytes.len() != 32 || code_hash_bytes.len() != 32 {
        return Err(MptError::BadAccountRlp);
    }
    let mut storage_root = [0u8; 32];
    storage_root.copy_from_slice(storage_root_bytes);
    let mut code_hash = [0u8; 32];
    code_hash.copy_from_slice(code_hash_bytes);

    Ok(Account {
        nonce: u64::from_be_bytes(nonce_buf),
        balance,
        storage_root,
        code_hash,
    })
}

/// Decodes a leaf/branch value slot's bytes as a storage scalar. Go-ethereum writes a storage value as
/// `rlp.EncodeToBytes(trimmedBigEndianBytes)` **before** ever calling `trie.Update` — so the bytes a
/// leaf's own RLP-string wrap unwraps to (`bytes` here) are *themselves* one more RLP string encoding the
/// real integer, not the integer's bytes directly (contrast [`decode_account_rlp`], whose value is
/// already list-shaped after one unwrap). This is the well-known double-RLP-encoding of storage trie
/// values; skipping the second unwrap silently misreads any value whose trimmed bytes start `>= 0x80`
/// (anything from 128 up) while happening to look right for the small values (`< 0x80`) a quick test
/// might reach for.
fn decode_storage_value(bytes: &[u8]) -> Result<[u8; 32], MptError> {
    let inner = rlp::decode_top_level_string(bytes).map_err(|_| MptError::BadAccountRlp)?;
    if inner.len() > 32 {
        return Err(MptError::ValueTooLong);
    }
    let mut out = [0u8; 32];
    out[32 - inner.len()..].copy_from_slice(inner);
    Ok(out)
}

/// The wire shape `ProveExit` carries: an account proof of the configured exit portal plus a
/// storage proof of `sentMessages[message_hash]` in that account's storage trie.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ExitProof {
    pub account_nodes: Vec<Vec<u8>>,
    pub storage_nodes: Vec<Vec<u8>>,
}

impl ExitProof {
    /// The proof's exact borsh-serialized byte length — what a caller budgets against the V1 envelope (the
    /// 4,096-byte limit) before ever building the instruction. Computed by actually serializing (borsh's own
    /// encoding, not a hand-derived formula that could drift from it).
    pub fn byte_len(&self) -> usize {
        borsh::to_vec(self)
            .expect("ExitProof (two Vec<Vec<u8>> fields) serializes infallibly")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn hex32(s: &str) -> [u8; 32] {
        let v = hex::decode(s.trim_start_matches("0x")).unwrap();
        v.try_into().unwrap()
    }

    fn hex20(s: &str) -> [u8; 20] {
        let v = hex::decode(s.trim_start_matches("0x")).unwrap();
        v.try_into().unwrap()
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        hex::decode(s.trim_start_matches("0x")).unwrap()
    }

    /// Parses an Ethereum JSON-RPC "quantity" hex string (leading zero *nibble* may be stripped, so the
    /// digit count can be odd) into a big-endian, left-padded 32-byte value — unlike [`hex_bytes`], which
    /// assumes a fixed-width, always-even-length byte string (a hash or an address).
    fn hex_quantity_be32(s: &str) -> [u8; 32] {
        let digits = s.trim_start_matches("0x");
        let padded = if digits.len() % 2 == 1 {
            format!("0{digits}")
        } else {
            digits.to_string()
        };
        let bytes = hex::decode(&padded).unwrap();
        let mut out = [0u8; 32];
        out[32 - bytes.len()..].copy_from_slice(&bytes);
        out
    }

    #[derive(Deserialize)]
    struct StorageProof {
        key: String,
        value: String,
        proof: Vec<String>,
    }

    #[derive(Deserialize)]
    struct GetProof {
        #[serde(rename = "accountProof")]
        account_proof: Vec<String>,
        #[serde(rename = "storageProof")]
        storage_proof: Vec<StorageProof>,
    }

    #[derive(Deserialize)]
    struct StateRoot {
        state_root: String,
    }

    #[derive(Deserialize)]
    struct MessageHash {
        portal: String,
    }

    #[derive(Deserialize)]
    struct ProofBalance {
        balance: String,
    }

    fn nodes_from_hex(hexes: &[String]) -> Vec<Vec<u8>> {
        hexes.iter().map(|h| hex_bytes(h)).collect()
    }

    const ANVIL_GET_PROOF: &str = include_str!("../../../fixtures/exit/anvil_getProof.json");
    const ANVIL_GET_PROOF_UNSENT: &str =
        include_str!("../../../fixtures/exit/anvil_getProof_unsent.json");
    const ANVIL_STATE_ROOT: &str = include_str!("../../../fixtures/exit/anvil_state_root.json");
    const MESSAGE_HASH: &str = include_str!("../../../fixtures/exit/message_hash.json");
    const EOA_GET_PROOF: &str = include_str!("../../../fixtures/exit/tiber_eoa_getProof.json");
    const EOA_BLOCK: &str = include_str!("../../../fixtures/exit/tiber_eoa_block.json");

    fn anvil_fixture() -> (GetProof, [u8; 32], [u8; 20]) {
        let proof: GetProof = serde_json::from_str(ANVIL_GET_PROOF).unwrap();
        let root: StateRoot = serde_json::from_str(ANVIL_STATE_ROOT).unwrap();
        let msg: MessageHash = serde_json::from_str(MESSAGE_HASH).unwrap();
        (proof, hex32(&root.state_root), hex20(&msg.portal))
    }

    #[test]
    fn anvil_fixture_verifies() {
        let (proof, state_root, portal) = anvil_fixture();
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();

        let storage_nodes = nodes_from_hex(&proof.storage_proof[0].proof);
        let slot = hex32(&proof.storage_proof[0].key);
        let value = verify_storage(&account.storage_root, &slot, &storage_nodes).unwrap();
        assert_eq!(proof.storage_proof[0].value, "0x1");
        assert_eq!(
            value,
            StorageValue::Present({
                let mut b = [0u8; 32];
                b[31] = 1;
                b
            })
        );
    }

    #[test]
    fn tiber_eoa_account_proof_verifies() {
        let proof: GetProof = serde_json::from_str(EOA_GET_PROOF).unwrap();
        #[derive(Deserialize)]
        struct Block {
            #[serde(rename = "stateRoot")]
            state_root: String,
        }
        let block: Block = serde_json::from_str(EOA_BLOCK).unwrap();
        let state_root = hex32(&block.state_root);

        #[derive(Deserialize)]
        struct ProofWithAddress {
            address: String,
        }
        let with_addr: ProofWithAddress = serde_json::from_str(EOA_GET_PROOF).unwrap();
        let address = hex20(&with_addr.address);

        let with_balance: ProofBalance = serde_json::from_str(EOA_GET_PROOF).unwrap();

        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &address, &account_nodes).unwrap();
        assert_eq!(account.nonce, 0);
        assert_eq!(account.balance, hex_quantity_be32(&with_balance.balance));
    }

    #[test]
    fn exclusion_proof_is_not_inclusion() {
        let (proof, state_root, portal) = anvil_fixture();
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();

        let unsent: GetProof = serde_json::from_str(ANVIL_GET_PROOF_UNSENT).unwrap();
        assert_eq!(unsent.storage_proof[0].value, "0x0");
        let storage_nodes = nodes_from_hex(&unsent.storage_proof[0].proof);
        let slot = hex32(&unsent.storage_proof[0].key);
        let value = verify_storage(&account.storage_root, &slot, &storage_nodes).unwrap();
        assert_eq!(value, StorageValue::Absent);
        assert_ne!(value, StorageValue::Present([0u8; 32]));
    }

    #[test]
    fn flipped_node_byte_is_refused() {
        let (proof, state_root, portal) = anvil_fixture();
        let account_nodes = nodes_from_hex(&proof.account_proof);
        for i in 0..account_nodes.len() {
            let mut mutated = account_nodes.clone();
            let last = mutated[i].len() - 1;
            mutated[i][last] ^= 0xff;
            assert!(
                verify_account(&state_root, &portal, &mutated).is_err(),
                "flipping account_nodes[{i}]'s last byte must be refused"
            );
        }

        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();
        let storage_nodes = nodes_from_hex(&proof.storage_proof[0].proof);
        let slot = hex32(&proof.storage_proof[0].key);
        for i in 0..storage_nodes.len() {
            let mut mutated = storage_nodes.clone();
            let last = mutated[i].len() - 1;
            mutated[i][last] ^= 0xff;
            assert!(
                verify_storage(&account.storage_root, &slot, &mutated).is_err(),
                "flipping storage_nodes[{i}]'s last byte must be refused"
            );
        }
    }

    #[test]
    fn proof_against_other_state_root_is_refused() {
        let (proof, _state_root, portal) = anvil_fixture();
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let wrong_root = [0x42u8; 32];
        assert_eq!(
            verify_account(&wrong_root, &portal, &account_nodes).unwrap_err(),
            MptError::RootMismatch
        );
    }

    #[test]
    fn proof_for_other_address_is_refused() {
        let (proof, state_root, _portal) = anvil_fixture();
        let account_nodes = nodes_from_hex(&proof.account_proof);
        // Ground-truth account tries have more than one populated branch slot (anvil's genesis funds ten
        // dev accounts), so an *arbitrary* wrong address usually diverges at a branch (`HashMismatch`),
        // not at the leaf — both are still refusals, but the test asks for `PathMismatch` specifically: the
        // leaf-comparison refusal, reached when the wrong address's key nibbles agree with the real
        // address's for exactly as many nibbles as the two branch nodes in this proof consume (2 nibbles
        // = the key's first byte), diverging only once the walk reaches the leaf itself. Ground-truthed by
        // grinding candidate 20-byte addresses for one whose `keccak256` shares the real portal key's
        // first byte (found at candidate 415 = 0x19f, off-line, not reproduced at test time).
        let other_address: [u8; 20] = hex20("0x000000000000000000000000000000000000019f");
        assert_eq!(
            verify_account(&state_root, &other_address, &account_nodes).unwrap_err(),
            MptError::PathMismatch
        );
    }

    #[test]
    fn node_over_532_bytes_refused() {
        let (proof, state_root, portal) = anvil_fixture();
        let mut account_nodes = nodes_from_hex(&proof.account_proof);
        account_nodes[0].resize(MAX_NODE_BYTES + 1, 0);
        assert_eq!(
            verify_account(&state_root, &portal, &account_nodes).unwrap_err(),
            MptError::NodeTooLarge {
                index: 0,
                len: MAX_NODE_BYTES + 1
            }
        );
    }

    #[test]
    fn more_than_64_nodes_refused() {
        let (proof, state_root, portal) = anvil_fixture();
        let mut account_nodes = nodes_from_hex(&proof.account_proof);
        while account_nodes.len() <= MAX_NODES {
            account_nodes.push(vec![0u8; 1]);
        }
        let got = account_nodes.len();
        assert_eq!(
            verify_account(&state_root, &portal, &account_nodes).unwrap_err(),
            MptError::TooManyNodes { got }
        );
    }

    #[test]
    fn trailing_node_refused() {
        let (proof, state_root, portal) = anvil_fixture();
        let mut account_nodes = nodes_from_hex(&proof.account_proof);
        account_nodes.push(vec![0x80]); // a well-formed but wholly unnecessary extra node
        assert_eq!(
            verify_account(&state_root, &portal, &account_nodes).unwrap_err(),
            MptError::TrailingNodes
        );
    }

    /// A hand-built two-node trie whose child is embedded inline (its own RLP encoding is under 32
    /// bytes): a root branch node with exactly one populated slot, referencing a leaf node directly by
    /// its raw list bytes rather than by a 32-byte keccak hash. Deterministic and independent of whatever
    /// a real 20-exit anvil trie happens to produce (`multi_fixture_has_branch_and_extension_nodes`
    /// measures that separately) — this proves the inline-child code path itself, byte for byte.
    #[test]
    fn inline_child_is_followed() {
        // Leaf: hex-prefix path (leaf, even) = 0x20, then one path byte 0xcd -> nibbles [c, d]; value
        // "hi" (2-byte string). RLP: c1-list-header, 0x82 path-len, 0x20 0xcd, 0x82 'h' 'i'.
        let leaf: Vec<u8> = {
            let path = [0x20u8, 0xcd]; // RLP short string, len 2
            let value = b"hi";
            let mut payload = Vec::new();
            payload.push(0x80 + path.len() as u8);
            payload.extend_from_slice(&path);
            payload.push(0x80 + value.len() as u8);
            payload.extend_from_slice(value);
            let mut out = Vec::new();
            out.push(0xc0 + payload.len() as u8);
            out.extend_from_slice(&payload);
            out
        };
        assert!(
            leaf.len() < 32,
            "the leaf must be small enough to embed inline"
        );

        // Root branch: 17 items, slot 5 = the leaf embedded as a nested list, everything else empty.
        let mut branch_payload = Vec::new();
        for i in 0..17u8 {
            if i == 5 {
                branch_payload.extend_from_slice(&leaf);
            } else {
                branch_payload.push(0x80); // empty string
            }
        }
        let mut root_node = Vec::new();
        assert!(branch_payload.len() < 56);
        root_node.push(0xc0 + branch_payload.len() as u8);
        root_node.extend_from_slice(&branch_payload);

        let root_hash = rome_zk_merkle::keccak256(&[root_node.as_slice()]);
        let nodes = vec![root_node];

        // Key nibbles: first nibble 5 (selects the inline slot), then [c, d] (the leaf's own path).
        let mut key_nibbles = vec![5u8, 0xc, 0xd];
        // walk() needs a full byte-derived key in real callers, but it only ever consumes `key_nibbles`
        // directly, so a hand-built nibble sequence (bypassing the address/slot keccak step) exercises
        // exactly the traversal logic under test, deterministically.
        let (outcome, consumed) = super::walk::walk(root_hash, &key_nibbles, &nodes).unwrap();
        assert_eq!(
            consumed, 1,
            "the inline child must not consume a second array entry"
        );
        match outcome {
            walk::WalkOutcome::Found(v) => assert_eq!(v, b"hi"),
            walk::WalkOutcome::Diverged => panic!("expected the inline leaf to be found"),
        }

        // A wrong final nibble diverges at the inline leaf, proving the inline node's own path really is
        // checked (not merely "present -> accept").
        key_nibbles[2] = 0xe;
        let (outcome, _) = super::walk::walk(root_hash, &key_nibbles, &nodes).unwrap();
        assert!(matches!(outcome, walk::WalkOutcome::Diverged));
    }

    const ANVIL_GET_PROOF_MULTI: &str =
        include_str!("../../../fixtures/exit/anvil_getProof_multi.json");
    const ANVIL_STATE_ROOT_MULTI: &str =
        include_str!("../../../fixtures/exit/anvil_state_root_multi.json");
    const MESSAGE_HASH_MULTI: &str = include_str!("../../../fixtures/exit/message_hash_multi.json");

    #[derive(Deserialize)]
    struct MultiFixture {
        portal: String,
        exit7_storage_slot: String,
        never_sent_storage_slot: String,
    }

    /// A node's shape, classified the same way `crate::walk` itself distinguishes them — independent of
    /// `crate::rlp`, so a bug in this crate's own decoder could not also fool this classification.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum NodeKind {
        Branch,
        Leaf,
        Extension,
    }

    fn classify_node(node: &[u8]) -> NodeKind {
        // Minimal, independent RLP walk: decode the outer list's item count and, for a 2-item node, the
        // hex-prefix terminator flag on its first item.
        fn decode_list(data: &[u8]) -> Vec<&[u8]> {
            let b0 = data[0];
            let (mut p, end) = if b0 <= 0xf7 {
                (1usize, 1 + (b0 - 0xc0) as usize)
            } else {
                let ll = (b0 - 0xf7) as usize;
                let mut len = 0usize;
                for &b in &data[1..1 + ll] {
                    len = (len << 8) | b as usize;
                }
                (1 + ll, 1 + ll + len)
            };
            let mut items = Vec::new();
            while p < end {
                let ib0 = data[p];
                let (istart, ilen, iconsumed) = if ib0 < 0x80 {
                    (p, 1, 1)
                } else if ib0 <= 0xb7 {
                    let l = (ib0 - 0x80) as usize;
                    (p + 1, l, 1 + l)
                } else if ib0 <= 0xbf {
                    let ll = (ib0 - 0xb7) as usize;
                    let mut l = 0usize;
                    for &b in &data[p + 1..p + 1 + ll] {
                        l = (l << 8) | b as usize;
                    }
                    (p + 1 + ll, l, 1 + ll + l)
                } else if ib0 <= 0xf7 {
                    let l = (ib0 - 0xc0) as usize;
                    (p, 1 + l, 1 + l)
                } else {
                    let ll = (ib0 - 0xf7) as usize;
                    let mut l = 0usize;
                    for &b in &data[p + 1..p + 1 + ll] {
                        l = (l << 8) | b as usize;
                    }
                    (p, 1 + ll + l, 1 + ll + l)
                };
                items.push(&data[istart..istart + ilen.min(data.len() - istart)]);
                p += iconsumed;
            }
            items
        }
        let items = decode_list(node);
        match items.len() {
            17 => NodeKind::Branch,
            2 => {
                if items[0].first().is_some_and(|b| b & 0x20 != 0) {
                    NodeKind::Leaf
                } else {
                    NodeKind::Extension
                }
            }
            n => panic!("unexpected node shape ({n} items) in a real getProof fixture"),
        }
    }

    /// The 20-exit anvil trie's actual node-kind composition, measured against the real fixture rather than
    /// assumed. **Contradicts the expected shape** (branch + extension nodes and likely inline
    /// children): with only 20 sparse keccak-derived keys spread over a 64-nibble keyspace, every populated
    /// branch's surviving children diverge from each other within the first couple of nibbles almost every time, so
    /// each subtree reaches a **leaf** directly — no two keys in this particular 20-key set share a long-enough
    /// common prefix to need an **extension** node, and no node here is small enough to embed **inline**. This is
    /// left as an open question, not silently corrected — the hand-built `inline_child_is_followed` test
    /// (deterministic, independent of what any real anvil trie produces) is what actually exercises the
    /// inline-child code path.
    #[test]
    fn multi_fixture_node_kinds_are_measured_not_assumed() {
        let proof: GetProof = serde_json::from_str(ANVIL_GET_PROOF_MULTI).unwrap();
        let root: StateRoot = serde_json::from_str(ANVIL_STATE_ROOT_MULTI).unwrap();
        let fixture: MultiFixture = serde_json::from_str(MESSAGE_HASH_MULTI).unwrap();

        let mut kinds = std::collections::BTreeSet::new();
        for n in &proof.account_proof {
            kinds.insert(classify_node(&hex_bytes(n)));
        }
        for sp in &proof.storage_proof {
            for n in &sp.proof {
                kinds.insert(classify_node(&hex_bytes(n)));
            }
        }
        assert!(
            kinds.contains(&NodeKind::Branch),
            "expected at least one branch node"
        );
        assert!(
            kinds.contains(&NodeKind::Leaf),
            "expected at least one leaf node"
        );
        assert!(
            !kinds.contains(&NodeKind::Extension),
            "measured fact, not an assumption: this 20-key trie has no extension node"
        );

        // End-to-end: the fixture still verifies correctly regardless of which node kinds it contains.
        let state_root = hex32(&root.state_root);
        let portal = hex20(&fixture.portal);
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();

        let exit7_slot = hex32(&fixture.exit7_storage_slot);
        let exit7_storage_nodes = nodes_from_hex(&proof.storage_proof[0].proof);
        assert_eq!(proof.storage_proof[0].value, "0x1");
        assert_eq!(
            verify_storage(&account.storage_root, &exit7_slot, &exit7_storage_nodes).unwrap(),
            StorageValue::Present({
                let mut b = [0u8; 32];
                b[31] = 1;
                b
            })
        );

        let never_sent_slot = hex32(&fixture.never_sent_storage_slot);
        let never_sent_storage_nodes = nodes_from_hex(&proof.storage_proof[1].proof);
        assert_eq!(proof.storage_proof[1].value, "0x0");
        assert_eq!(
            verify_storage(
                &account.storage_root,
                &never_sent_slot,
                &never_sent_storage_nodes
            )
            .unwrap(),
            StorageValue::Absent
        );
    }

    const ANVIL_GET_PROOF_EXT: &str =
        include_str!("../../../fixtures/exit/anvil_getProof_ext.json");
    const ANVIL_STATE_ROOT_EXT: &str =
        include_str!("../../../fixtures/exit/anvil_state_root_ext.json");
    const MESSAGE_HASH_EXT: &str = include_str!("../../../fixtures/exit/message_hash_ext.json");

    #[derive(Deserialize)]
    struct ExtFixture {
        portal: String,
    }

    /// The extension-node walk (the classic forge vector — a forged proof whose path skips an extension's shared
    /// nibbles) had zero committed regression coverage. This 500-`initiateExit` fixture
    /// (`fixtures/exit/README.md`'s provenance) reliably contains real Extension nodes — asserted here, not
    /// assumed, the same discipline `multi_fixture_node_kinds_…` already applies to the 20-exit fixture (which
    /// measured none).
    #[test]
    fn ext_fixture_has_an_extension_node() {
        let proof: GetProof = serde_json::from_str(ANVIL_GET_PROOF_EXT).unwrap();
        let mut kinds = std::collections::BTreeSet::new();
        for n in &proof.account_proof {
            kinds.insert(classify_node(&hex_bytes(n)));
        }
        for sp in &proof.storage_proof {
            for n in &sp.proof {
                kinds.insert(classify_node(&hex_bytes(n)));
            }
        }
        assert!(
            kinds.contains(&NodeKind::Extension),
            "the ext fixture must contain at least one real Extension node"
        );
    }

    /// The fixture's sent slot (nonce 21) is the first of 500 whose storage proof actually traverses a
    /// real Extension node before reaching its leaf (Branch, Branch, Extension, Branch, Leaf) — this is
    /// what catches mutation MA (dropping `walk.rs`'s extension path-consume): without the consume, the
    /// subsequent branch step reads the wrong nibble and this proof no longer verifies to `Present(1)`.
    #[test]
    fn inclusion_through_extension_verifies() {
        let proof: GetProof = serde_json::from_str(ANVIL_GET_PROOF_EXT).unwrap();
        let root: StateRoot = serde_json::from_str(ANVIL_STATE_ROOT_EXT).unwrap();
        let fixture: ExtFixture = serde_json::from_str(MESSAGE_HASH_EXT).unwrap();
        let state_root = hex32(&root.state_root);
        let portal = hex20(&fixture.portal);
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();

        let sent = &proof.storage_proof[0];
        let sent_kinds: Vec<NodeKind> = sent
            .proof
            .iter()
            .map(|n| classify_node(&hex_bytes(n)))
            .collect();
        assert!(
            sent_kinds.contains(&NodeKind::Extension),
            "fixture drift: the sent slot's proof no longer traverses an Extension node ({sent_kinds:?})"
        );
        assert_eq!(sent.value, "0x1");

        let slot = hex32(&sent.key);
        let storage_nodes = nodes_from_hex(&sent.proof);
        assert_eq!(
            verify_storage(&account.storage_root, &slot, &storage_nodes).unwrap(),
            StorageValue::Present({
                let mut b = [0u8; 32];
                b[31] = 1;
                b
            })
        );
    }

    /// The fixture's never-sent slot (nonce 516) is the first of 250 candidates whose exclusion proof
    /// terminates (diverges) AT a real Extension node — the walk needs no further node once it finds the
    /// extension's own single nibble does not match the key, proving the extension arm's divergence path
    /// (not just its consume-and-continue path) refuses cleanly rather than defaulting to some other
    /// outcome.
    #[test]
    fn exclusion_diverging_inside_extension_is_absent() {
        let proof: GetProof = serde_json::from_str(ANVIL_GET_PROOF_EXT).unwrap();
        let root: StateRoot = serde_json::from_str(ANVIL_STATE_ROOT_EXT).unwrap();
        let fixture: ExtFixture = serde_json::from_str(MESSAGE_HASH_EXT).unwrap();
        let state_root = hex32(&root.state_root);
        let portal = hex20(&fixture.portal);
        let account_nodes = nodes_from_hex(&proof.account_proof);
        let account = verify_account(&state_root, &portal, &account_nodes).unwrap();

        let unsent = &proof.storage_proof[1];
        let unsent_kinds: Vec<NodeKind> = unsent
            .proof
            .iter()
            .map(|n| classify_node(&hex_bytes(n)))
            .collect();
        assert_eq!(
            unsent_kinds.last(),
            Some(&NodeKind::Extension),
            "fixture drift: the unsent slot's proof no longer diverges at an Extension node ({unsent_kinds:?})"
        );
        assert_eq!(unsent.value, "0x0");

        let slot = hex32(&unsent.key);
        let storage_nodes = nodes_from_hex(&unsent.proof);
        let value = verify_storage(&account.storage_root, &slot, &storage_nodes).unwrap();
        assert_eq!(value, StorageValue::Absent);
        assert_ne!(value, StorageValue::Present([0u8; 32]));
    }

    /// Minimal canonical RLP string/list encoders, mirroring `crate::rlp`'s own short/long-form rules
    /// (short form for a payload ≤ 55 bytes, long form with a trimmed big-endian length-of-length
    /// otherwise) — used only to hand-build hash-consistent single-node trie fixtures below, never
    /// exercising `crate::rlp` itself (these tests must stay independent of the decoder under test).
    fn rlp_string(payload: &[u8]) -> Vec<u8> {
        if payload.len() == 1 && payload[0] < 0x80 {
            return payload.to_vec();
        }
        let mut out = Vec::new();
        if payload.len() <= 55 {
            out.push(0x80 + payload.len() as u8);
        } else {
            let mut len_bytes = payload.len().to_be_bytes().to_vec();
            while len_bytes.len() > 1 && len_bytes[0] == 0 {
                len_bytes.remove(0);
            }
            out.push(0xb7 + len_bytes.len() as u8);
            out.extend_from_slice(&len_bytes);
        }
        out.extend_from_slice(payload);
        out
    }

    fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.iter().flatten().copied().collect();
        let mut out = Vec::new();
        if payload.len() <= 55 {
            out.push(0xc0 + payload.len() as u8);
        } else {
            let mut len_bytes = payload.len().to_be_bytes().to_vec();
            while len_bytes.len() > 1 && len_bytes[0] == 0 {
                len_bytes.remove(0);
            }
            out.push(0xf7 + len_bytes.len() as u8);
            out.extend_from_slice(&len_bytes);
        }
        out.extend_from_slice(&payload);
        out
    }

    /// Hand-built, hash-consistent 3-nibble extension node (no fixture needed): covers three shapes the real
    /// 500-exit fixture's own single-nibble extensions cannot exercise on their own. Path nibbles `[1,2,3]` (odd
    /// count, so the hex-prefix flag byte folds nibble `1` in: `0x11`, then `0x23` for the remaining pair), child a
    /// 32-byte hash reference the `nodes` array never actually carries.
    #[test]
    fn hand_built_extension_divergence_and_missing_child() {
        let path_bytes = [0x11u8, 0x23u8];
        let child_hash = [0x77u8; 32];
        let node = rlp_list(&[rlp_string(&path_bytes), rlp_string(&child_hash)]);
        let root = rome_zk_merkle::keccak256(&[node.as_slice()]);
        let nodes = vec![node];

        // Divergence strictly inside the path (middle nibble 9 != 2).
        let (outcome, consumed) = super::walk::walk(root, &[1u8, 9, 3], &nodes).unwrap();
        assert_eq!(consumed, 1);
        assert!(matches!(outcome, walk::WalkOutcome::Diverged));

        // The key ends inside the path (remaining shorter than the path) — must be refused (Diverged),
        // never panic on an out-of-bounds slice (proves the length guard runs before the byte compare).
        let (outcome, consumed) = super::walk::walk(root, &[1u8, 2], &nodes).unwrap();
        assert_eq!(consumed, 1);
        assert!(matches!(outcome, walk::WalkOutcome::Diverged));

        // A full match into a child the proof does not carry: BadRlp{index:1}, not a panic, not a false
        // Found. (Matched by hand, not `.unwrap_err()`: `WalkOutcome` has no `Debug` impl and this test
        // must not add one to `walk.rs`.)
        match super::walk::walk(root, &[1u8, 2, 3], &nodes) {
            Err(e) => assert_eq!(e, MptError::BadRlp { index: 1 }),
            Ok(_) => panic!("expected BadRlp{{index:1}} (a full match into an uncarried child)"),
        }
    }

    /// The storage-value length guard (`decode_storage_value`'s `inner.len() > 32`) had zero committed test. A
    /// hand-built single-leaf storage trie whose double-RLP-wrapped value is 33 bytes.
    #[test]
    fn storage_value_over_32_bytes_is_refused() {
        let slot = [0u8; 32];
        let key = rome_zk_merkle::keccak256(&[slot.as_slice()]);
        let mut path_bytes = vec![0x20u8]; // leaf, even, zero padding
        path_bytes.extend_from_slice(&key);

        // Double-RLP: the inner RLP string (go-ethereum's own pre-store encoding) wraps a 33-byte
        // payload — one byte over the 32-byte scalar this crate accepts.
        let inner_encoded = rlp_string(&[0xABu8; 33]);
        let leaf = rlp_list(&[rlp_string(&path_bytes), rlp_string(&inner_encoded)]);
        let root = rome_zk_merkle::keccak256(&[leaf.as_slice()]);
        let nodes = vec![leaf];

        assert_eq!(
            verify_storage(&root, &slot, &nodes).unwrap_err(),
            MptError::ValueTooLong
        );
    }

    /// A non-canonical leading-zero storage value (`0x00 01`) must still decode to the correct left-padded
    /// `Present(1)` — `ProveExit`'s `== 1` compare relies on this.
    #[test]
    fn leading_zero_storage_value_decodes_to_one() {
        let slot = [0x11u8; 32];
        let key = rome_zk_merkle::keccak256(&[slot.as_slice()]);
        let mut path_bytes = vec![0x20u8];
        path_bytes.extend_from_slice(&key);

        let inner_encoded = rlp_string(&[0x00u8, 0x01u8]);
        let leaf = rlp_list(&[rlp_string(&path_bytes), rlp_string(&inner_encoded)]);
        let root = rome_zk_merkle::keccak256(&[leaf.as_slice()]);
        let nodes = vec![leaf];

        assert_eq!(
            verify_storage(&root, &slot, &nodes).unwrap(),
            StorageValue::Present({
                let mut b = [0u8; 32];
                b[31] = 1;
                b
            })
        );
    }

    /// `decode_account_rlp`'s defensive shape/length guards, hand-built one adversarial case at a time (each its
    /// own hash-consistent single-leaf trie) — every one refused by name.
    #[test]
    fn account_rlp_adversarial_refused() {
        let address = [0x22u8; 20];
        let key = rome_zk_merkle::keccak256(&[address.as_slice()]);
        let mut path_bytes = vec![0x20u8];
        path_bytes.extend_from_slice(&key);
        let path_item = rlp_string(&path_bytes);

        let valid_storage_root = [0xAAu8; 32];
        let valid_code_hash = [0xBBu8; 32];

        let verify = |account_rlp: Vec<u8>| -> MptError {
            let leaf = rlp_list(&[path_item.clone(), rlp_string(&account_rlp)]);
            let root = rome_zk_merkle::keccak256(&[leaf.as_slice()]);
            let nodes = vec![leaf];
            verify_account(&root, &address, &nodes).unwrap_err()
        };

        // nonce > 8 bytes.
        let nonce_too_long = rlp_list(&[
            rlp_string(&[0xAAu8; 9]),
            rlp_string(&[0x01]),
            rlp_string(&valid_storage_root),
            rlp_string(&valid_code_hash),
        ]);
        assert_eq!(verify(nonce_too_long), MptError::ValueTooLong);

        // balance > 32 bytes.
        let balance_too_long = rlp_list(&[
            rlp_string(&[0x01]),
            rlp_string(&[0xAAu8; 33]),
            rlp_string(&valid_storage_root),
            rlp_string(&valid_code_hash),
        ]);
        assert_eq!(verify(balance_too_long), MptError::ValueTooLong);

        // storage_root not exactly 32 bytes.
        let bad_storage_root = rlp_list(&[
            rlp_string(&[0x01]),
            rlp_string(&[0x01]),
            rlp_string(&[0xAAu8; 31]),
            rlp_string(&valid_code_hash),
        ]);
        assert_eq!(verify(bad_storage_root), MptError::BadAccountRlp);

        // a 3-item list (missing code_hash).
        let three_items = rlp_list(&[
            rlp_string(&[0x01]),
            rlp_string(&[0x01]),
            rlp_string(&valid_storage_root),
        ]);
        assert_eq!(verify(three_items), MptError::BadAccountRlp);

        // a nested scalar: balance is itself a (empty) list, not a string.
        let nested_scalar = rlp_list(&[
            rlp_string(&[0x01]),
            rlp_list(&[]),
            rlp_string(&valid_storage_root),
            rlp_string(&valid_code_hash),
        ]);
        assert_eq!(verify(nested_scalar), MptError::BadAccountRlp);
    }

    /// A non-canonical even-path hex-prefix byte (padding nibble nonzero) must be refused, not silently accepted
    /// with the padding discarded. Before the `nibbles::decode_compact_path` guard this leaf verified to
    /// `Present(1)` anyway — HP byte `0x2f` (even/leaf flag `0x2`, nonzero low nibble `0xf`) instead of the
    /// canonical `0x20`.
    #[test]
    fn non_canonical_even_hp_padding_is_refused() {
        let slot = [0x33u8; 32];
        let key = rome_zk_merkle::keccak256(&[slot.as_slice()]);
        let mut path_bytes = vec![0x2fu8]; // non-canonical: even/leaf flag, but a nonzero padding nibble
        path_bytes.extend_from_slice(&key);

        let leaf = rlp_list(&[rlp_string(&path_bytes), rlp_string(&[0x01])]);
        let root = rome_zk_merkle::keccak256(&[leaf.as_slice()]);
        let nodes = vec![leaf];

        assert!(
            verify_storage(&root, &slot, &nodes).is_err(),
            "a non-canonical even-path HP padding nibble must be refused, not accepted as Present"
        );
    }

    #[test]
    fn byte_len_matches_actual_borsh_encoding() {
        let proof = ExitProof {
            account_nodes: vec![vec![1, 2, 3], vec![4, 5]],
            storage_nodes: vec![vec![6]],
        };
        assert_eq!(proof.byte_len(), borsh::to_vec(&proof).unwrap().len());
    }

    #[test]
    fn exit_proof_borsh_round_trips() {
        let proof = ExitProof {
            account_nodes: vec![vec![1, 2, 3]],
            storage_nodes: vec![vec![4, 5, 6], vec![7]],
        };
        let bytes = borsh::to_vec(&proof).unwrap();
        let back: ExitProof = borsh::from_slice(&bytes).unwrap();
        assert_eq!(proof, back);
    }
}
