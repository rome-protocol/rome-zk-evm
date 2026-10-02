//! Exits: the exit message (its ABI preimage, hash
//! and target storage slot — pinned byte-for-byte to `contracts/exit-portal`'s Solidity and to
//! `fixtures/exit/message_hash.json`), the nullifier-page bit math, the cap-unit conversion, and the four
//! new zk-settlement account layouts `ProveExit`/`ConsumeExit`/`ProposeExitConfig`/`ActivateExitConfig`
//! (the instruction handlers in `programs/zk-settlement`) read and write: `exit_config`, `exit_record`, `exit_window`,
//! `exit_nullifier`.
//!
//! **Where verification itself lives:** this module defines byte layouts and pure arithmetic only — no
//! MPT proof, no on-chain instruction handler. `crates/rome-zk-mpt` (a sibling crate) proves
//! an exit's inclusion against a `state_root`; `programs/zk-settlement`'s `ProveExit` is what
//! actually calls both.
//!
//! **Exit message ABI:** `RomeExitPortal.initiateExit` (`contracts/exit-portal/src`) commits
//! `keccak256(abi.encode(uint256 nonce, address l2Sender, bytes32 solRecipient, address asset, uint256
//! amount))` — a 160-byte preimage, five 32-byte words — to `sentMessages[messageHash]` at storage slot 0
//! (the mapping's own storage slot); [`ExitMessage::message_hash`]/[`ExitMessage::storage_slot`] compute
//! the exact same bytes off-chain, pinned against that contract's own fixture
//! (`fixtures/exit/message_hash.json`, produced with the contract) so a future ABI drift on either side is caught here.
//!
//! **Nullifier paging:** replay protection is one bit per exit, `nonce >> 13` selects which
//! 8,192-exit page a nonce's bit lives on (`nullifier_page`/`nullifier_bit`), persisted forever (unlike
//! `exit_record`, which `ConsumeExit` recycles) — a page is rent paid once, by whichever prover is first
//! to touch it.
//!
//! **Cap unit:** `exit_cap_per_window` (the root account's existing field, `root.rs`) counts
//! in whole gwei of the native asset; [`cap_units`] converts a wei amount to that unit, rounding up (a
//! sub-gwei remainder still costs a whole unit against the window's cap — conservative, never lets a
//! flood of dust-remainder exits slip under the cap for free).

/// One gwei, in wei — the unit `exit_cap_per_window` (root account) and `exit_window::spent_cap_units`
/// both count in.
pub const CAP_UNIT_WEI: u128 = 1_000_000_000;

/// A wei amount exceeded what [`cap_units`] can express as a `u64` count of [`CAP_UNIT_WEI`] units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitAmountOverflow;

/// Converts a wei amount to whole [`CAP_UNIT_WEI`] units, rounding **up** — a sub-gwei remainder still
/// costs one whole unit against a window's cap (conservative: never lets dust remainders slip under the
/// cap for free). Refuses (rather than truncating) an amount whose unit count does not fit in a `u64`.
pub fn cap_units(amount_wei: u128) -> Result<u64, ExitAmountOverflow> {
    let units = amount_wei.div_ceil(CAP_UNIT_WEI);
    u64::try_from(units).map_err(|_| ExitAmountOverflow)
}

/// Number of low bits of a nonce that select its bit *within* a nullifier page — `2^13 = 8,192` exits per
/// page.
const NULLIFIER_PAGE_BITS: u32 = 13;

/// `2^`[`NULLIFIER_PAGE_BITS`] — how many distinct nonces share one `exit_nullifier` page.
pub const NULLIFIER_EXITS_PER_PAGE: u64 = 1 << NULLIFIER_PAGE_BITS;

/// Which `exit_nullifier` page a nonce's replay bit lives on.
pub fn nullifier_page(nonce: u64) -> u64 {
    nonce >> NULLIFIER_PAGE_BITS
}

/// Which bit, within its page's [`exit_nullifier::BITS_LEN`]-byte bitmap, a nonce's replay bit is.
pub fn nullifier_bit(nonce: u64) -> u64 {
    nonce & (NULLIFIER_EXITS_PER_PAGE - 1)
}

/// A caller passed a `(page, nonce)` pair where `page != nullifier_page(nonce)` to [`bit_is_set`]/
/// [`set_bit`] — the same refusal `programs/zk-settlement`'s `ProveExit` surfaces when an
/// account's own derived page does not match the message's nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NullifierPageMismatch;

/// Reads whether `nonce`'s replay bit is set in `bits` (an `exit_nullifier` account's
/// [`exit_nullifier::BITS_LEN`]-byte bitmap region — the caller slices `&account_data[exit_nullifier::OFF_BITS..]`
/// itself). Refuses [`NullifierPageMismatch`] before touching `bits` at all if `page` does not actually
/// own this nonce's bit — a caller cannot query (or, via [`set_bit`], set) the wrong page's copy of a bit
/// by passing a mismatched page number.
pub fn bit_is_set(bits: &[u8], page: u64, nonce: u64) -> Result<bool, NullifierPageMismatch> {
    if nullifier_page(nonce) != page {
        return Err(NullifierPageMismatch);
    }
    let bit = nullifier_bit(nonce) as usize;
    Ok((bits[bit / 8] >> (bit % 8)) & 1 == 1)
}

/// Sets `nonce`'s replay bit in `bits` — see [`bit_is_set`] for the page-ownership check both share.
pub fn set_bit(bits: &mut [u8], page: u64, nonce: u64) -> Result<(), NullifierPageMismatch> {
    if nullifier_page(nonce) != page {
        return Err(NullifierPageMismatch);
    }
    let bit = nullifier_bit(nonce) as usize;
    bits[bit / 8] |= 1 << (bit % 8);
    Ok(())
}

/// `["exit_consumer", chain_id]` — the PDA `ConsumeExit`'s whole authorisation check derives against,
/// UNDER `exit_config.bridge_program` (never under the settlement program's own id, and never a
/// caller-supplied address). This is the one seed pair both sides of the bridge relationship — this
/// program's `ConsumeExit` and, later, the real `zk-bridge` program — must derive
/// identically; `zk-bridge`'s own `ReleaseExit` CPIs `ConsumeExit` signing exactly this PDA (via
/// `invoke_signed`), which only that program's own runtime identity can ever produce. A keypair can never
/// sign as a PDA, so this is the entire mechanism by which "only the registered bridge may release" holds.
#[inline]
pub fn exit_consumer_seeds(chain_id: u64) -> [Vec<u8>; 2] {
    [b"exit_consumer".to_vec(), chain_id.to_le_bytes().to_vec()]
}

/// Derives the `exit_consumer` PDA UNDER `bridge_program` (not the settlement program — the caller passes
/// whichever program id it means to check against, matching `ConsumeExit`'s own
/// `exit_config.bridge_program` read).
#[cfg(feature = "solana")]
#[inline]
pub fn exit_consumer_pda(
    chain_id: u64,
    bridge_program: &solana_program::pubkey::Pubkey,
) -> (solana_program::pubkey::Pubkey, u8) {
    let s = exit_consumer_seeds(chain_id);
    solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], bridge_program)
}

/// The exit message `RomeExitPortal.initiateExit` commits — the fields
/// [`ExitMessage::message_preimage`]'s 160-byte ABI encoding carries, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitMessage {
    pub nonce: u64,
    pub l2_sender: [u8; 20],
    pub sol_recipient: [u8; 32],
    pub asset: [u8; 20],
    /// v1 native-asset exits are u128-bounded (`RomeExitPortal` refuses `msg.value > type(uint128).max`);
    /// the ABI word itself is a full uint256, zero-extended from this value (see
    /// [`ExitMessage::message_preimage`]).
    pub amount: u128,
}

impl ExitMessage {
    /// `abi.encode(uint256 nonce, address l2Sender, bytes32 solRecipient, address asset, uint256
    /// amount)` — five 32-byte words: `nonce` and `amount` right-aligned (big-endian) in their word,
    /// `l2_sender`/`asset` right-aligned in their word (a Solidity `address` ABI-encodes as
    /// 12-zero-bytes-then-20), `sol_recipient` filling its word exactly (already 32 bytes). Pinned
    /// byte-for-byte against `fixtures/exit/message_hash.json`'s `preimage_hex`
    /// (`preimage_is_160_bytes_and_matches_fixture`).
    pub fn message_preimage(&self) -> [u8; 160] {
        let mut out = [0u8; 160];
        out[24..32].copy_from_slice(&self.nonce.to_be_bytes());
        out[44..64].copy_from_slice(&self.l2_sender);
        out[64..96].copy_from_slice(&self.sol_recipient);
        out[108..128].copy_from_slice(&self.asset);
        out[144..160].copy_from_slice(&self.amount.to_be_bytes());
        out
    }

    /// `keccak256(message_preimage())` — the value `sentMessages` is keyed by and `ProveExit`
    /// re-derives from the message it is proving.
    pub fn message_hash(&self) -> [u8; 32] {
        rome_zk_merkle::keccak256(&[&self.message_preimage()])
    }

    /// `keccak256(pad32(message_hash) ‖ pad32(0))` — `sentMessages`'s storage slot for this message
    /// (`sentMessages` is declared at storage slot 0, so a `mapping(bytes32 => bool)` entry's slot is
    /// `keccak256(abi.encode(key, uint256(0)))`, per Solidity's storage layout rules).
    pub fn storage_slot(&self) -> [u8; 32] {
        let hash = self.message_hash();
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&hash);
        rome_zk_merkle::keccak256(&[&preimage])
    }
}

/// The `exit_config` account (`["exit_config", chain_id]`, magic `"ZKXC"`, `LEN` 142) — the portal
/// address, bridge program, and pending exit-cap/poster-bond values a chain's exit machinery reads,
/// written by `ProposeExitConfig`/`ActivateExitConfig` with an activation delay. Absent account
/// = exits disabled for that chain.
pub mod exit_config {
    pub const MAGIC: u32 = 0x5a4b_5843; // "ZKXC"
    pub const VERSION: u8 = 1;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_CHAIN_ID: usize = 5;
    pub const OFF_EXIT_PORTAL: usize = 13;
    pub const OFF_BRIDGE_PROGRAM: usize = 33;
    pub const OFF_PENDING_EXIT_PORTAL: usize = 65;
    pub const OFF_PENDING_BRIDGE_PROGRAM: usize = 85;
    pub const OFF_PENDING_EXIT_CAP: usize = 117;
    pub const OFF_PENDING_POSTER_BOND: usize = 125;
    pub const OFF_ACTIVATION_SLOT: usize = 133;
    pub const OFF_PENDING_MASK: usize = 141;
    /// Full fixed-size account length.
    pub const LEN: usize = 142;

    /// `pending_mask` bit positions (`ProposeExitConfig` sets these; `ActivateExitConfig`
    /// clears them on activation).
    pub const PENDING_MASK_PORTAL: u8 = 1 << 0;
    pub const PENDING_MASK_BRIDGE: u8 = 1 << 1;
    pub const PENDING_MASK_CAP: u8 = 1 << 2;
    pub const PENDING_MASK_BOND: u8 = 1 << 3;

    /// `["exit_config", chain_id]`.
    #[inline]
    pub fn seeds(chain_id: u64) -> [Vec<u8>; 2] {
        [b"exit_config".to_vec(), chain_id.to_le_bytes().to_vec()]
    }

    /// Derives the `exit_config` PDA under `program_id` (the settlement program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        chain_id: u64,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(chain_id);
        solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1]], program_id)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExitConfigFields {
        pub chain_id: u64,
        pub exit_portal: [u8; 20],
        pub bridge_program: [u8; 32],
        pub pending_exit_portal: [u8; 20],
        pub pending_bridge_program: [u8; 32],
        pub pending_exit_cap: u64,
        pub pending_poster_bond: u64,
        pub activation_slot: u64,
        pub pending_mask: u8,
    }

    pub fn read(d: &[u8]) -> Result<ExitConfigFields, crate::LayoutError> {
        if d.len() < LEN {
            return Err(crate::LayoutError::TooShort {
                need: LEN,
                got: d.len(),
            });
        }
        if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
            return Err(crate::LayoutError::BadMagic);
        }
        if d[OFF_VERSION] != VERSION {
            return Err(crate::LayoutError::BadVersion);
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let b20_at = |o: usize| -> [u8; 20] { d[o..o + 20].try_into().unwrap() };
        let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
        Ok(ExitConfigFields {
            chain_id: u64_at(OFF_CHAIN_ID),
            exit_portal: b20_at(OFF_EXIT_PORTAL),
            bridge_program: b32_at(OFF_BRIDGE_PROGRAM),
            pending_exit_portal: b20_at(OFF_PENDING_EXIT_PORTAL),
            pending_bridge_program: b32_at(OFF_PENDING_BRIDGE_PROGRAM),
            pending_exit_cap: u64_at(OFF_PENDING_EXIT_CAP),
            pending_poster_bond: u64_at(OFF_PENDING_POSTER_BOND),
            activation_slot: u64_at(OFF_ACTIVATION_SLOT),
            pending_mask: d[OFF_PENDING_MASK],
        })
    }

    pub fn write(f: &ExitConfigFields) -> [u8; LEN] {
        let mut d = [0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
        d[OFF_EXIT_PORTAL..OFF_EXIT_PORTAL + 20].copy_from_slice(&f.exit_portal);
        d[OFF_BRIDGE_PROGRAM..OFF_BRIDGE_PROGRAM + 32].copy_from_slice(&f.bridge_program);
        d[OFF_PENDING_EXIT_PORTAL..OFF_PENDING_EXIT_PORTAL + 20]
            .copy_from_slice(&f.pending_exit_portal);
        d[OFF_PENDING_BRIDGE_PROGRAM..OFF_PENDING_BRIDGE_PROGRAM + 32]
            .copy_from_slice(&f.pending_bridge_program);
        d[OFF_PENDING_EXIT_CAP..OFF_PENDING_EXIT_CAP + 8]
            .copy_from_slice(&f.pending_exit_cap.to_le_bytes());
        d[OFF_PENDING_POSTER_BOND..OFF_PENDING_POSTER_BOND + 8]
            .copy_from_slice(&f.pending_poster_bond.to_le_bytes());
        d[OFF_ACTIVATION_SLOT..OFF_ACTIVATION_SLOT + 8]
            .copy_from_slice(&f.activation_slot.to_le_bytes());
        d[OFF_PENDING_MASK] = f.pending_mask;
        d
    }
}

/// The `exit_record` account (`["exit", chain_id, message_hash]`, magic `"ZKEX"`, `LEN` 169) — one per
/// proved-but-not-yet-released exit, created by `ProveExit` and recycled by `ConsumeExit`.
pub mod exit_record {
    pub const MAGIC: u32 = 0x5a4b_4558; // "ZKEX"
    pub const STATUS_PROVED: u8 = 1;
    pub const STATUS_RELEASED: u8 = 2;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_CHAIN_ID: usize = 4;
    pub const OFF_BATCH: usize = 12;
    pub const OFF_MESSAGE_HASH: usize = 20;
    pub const OFF_SOL_RECIPIENT: usize = 52;
    pub const OFF_AMOUNT: usize = 84;
    pub const OFF_WINDOW_INDEX: usize = 100;
    pub const OFF_PROVED_SLOT: usize = 108;
    pub const OFF_STATUS: usize = 116;
    pub const OFF_PAYER: usize = 117;
    pub const OFF_ASSET: usize = 149;
    /// Full fixed-size account length.
    pub const LEN: usize = 169;

    /// `["exit", chain_id, message_hash]`.
    #[inline]
    pub fn seeds(chain_id: u64, message_hash: [u8; 32]) -> [Vec<u8>; 3] {
        [
            b"exit".to_vec(),
            chain_id.to_le_bytes().to_vec(),
            message_hash.to_vec(),
        ]
    }

    /// Derives the `exit_record` PDA under `program_id` (the settlement program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        chain_id: u64,
        message_hash: [u8; 32],
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(chain_id, message_hash);
        solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExitRecordFields {
        pub chain_id: u64,
        pub batch: u64,
        pub message_hash: [u8; 32],
        pub sol_recipient: [u8; 32],
        pub amount: u128,
        pub window_index: u64,
        pub proved_slot: u64,
        pub status: u8,
        pub payer: [u8; 32],
        pub asset: [u8; 20],
    }

    pub fn read(d: &[u8]) -> Result<ExitRecordFields, crate::LayoutError> {
        if d.len() < LEN {
            return Err(crate::LayoutError::TooShort {
                need: LEN,
                got: d.len(),
            });
        }
        if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
            return Err(crate::LayoutError::BadMagic);
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let u128_at = |o: usize| u128::from_le_bytes(d[o..o + 16].try_into().unwrap());
        let b20_at = |o: usize| -> [u8; 20] { d[o..o + 20].try_into().unwrap() };
        let b32_at = |o: usize| -> [u8; 32] { d[o..o + 32].try_into().unwrap() };
        Ok(ExitRecordFields {
            chain_id: u64_at(OFF_CHAIN_ID),
            batch: u64_at(OFF_BATCH),
            message_hash: b32_at(OFF_MESSAGE_HASH),
            sol_recipient: b32_at(OFF_SOL_RECIPIENT),
            amount: u128_at(OFF_AMOUNT),
            window_index: u64_at(OFF_WINDOW_INDEX),
            proved_slot: u64_at(OFF_PROVED_SLOT),
            status: d[OFF_STATUS],
            payer: b32_at(OFF_PAYER),
            asset: b20_at(OFF_ASSET),
        })
    }

    pub fn write(f: &ExitRecordFields) -> [u8; LEN] {
        let mut d = [0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
        d[OFF_BATCH..OFF_BATCH + 8].copy_from_slice(&f.batch.to_le_bytes());
        d[OFF_MESSAGE_HASH..OFF_MESSAGE_HASH + 32].copy_from_slice(&f.message_hash);
        d[OFF_SOL_RECIPIENT..OFF_SOL_RECIPIENT + 32].copy_from_slice(&f.sol_recipient);
        d[OFF_AMOUNT..OFF_AMOUNT + 16].copy_from_slice(&f.amount.to_le_bytes());
        d[OFF_WINDOW_INDEX..OFF_WINDOW_INDEX + 8].copy_from_slice(&f.window_index.to_le_bytes());
        d[OFF_PROVED_SLOT..OFF_PROVED_SLOT + 8].copy_from_slice(&f.proved_slot.to_le_bytes());
        d[OFF_STATUS] = f.status;
        d[OFF_PAYER..OFF_PAYER + 32].copy_from_slice(&f.payer);
        d[OFF_ASSET..OFF_ASSET + 20].copy_from_slice(&f.asset);
        d
    }
}

/// The `exit_window` account (`["exit_window", chain_id, window_index]`, magic `"ZKEW"`, `LEN` 36) — one
/// per challenge-window-length time bucket, tracking how many [`CAP_UNIT_WEI`] units this chain's exits
/// have spent against `exit_cap_per_window` (root account) so far in that window.
pub mod exit_window {
    pub const MAGIC: u32 = 0x5a4b_4557; // "ZKEW"

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_CHAIN_ID: usize = 4;
    pub const OFF_WINDOW_INDEX: usize = 12;
    pub const OFF_SPENT_CAP_UNITS: usize = 20;
    pub const OFF_EXITS: usize = 28;
    pub const OFF_PAD: usize = 32;
    /// Full fixed-size account length.
    pub const LEN: usize = 36;

    /// `["exit_window", chain_id, window_index]`.
    #[inline]
    pub fn seeds(chain_id: u64, window_index: u64) -> [Vec<u8>; 3] {
        [
            b"exit_window".to_vec(),
            chain_id.to_le_bytes().to_vec(),
            window_index.to_le_bytes().to_vec(),
        ]
    }

    /// Derives the `exit_window` PDA under `program_id` (the settlement program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        chain_id: u64,
        window_index: u64,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(chain_id, window_index);
        solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ExitWindowFields {
        pub chain_id: u64,
        pub window_index: u64,
        pub spent_cap_units: u64,
        pub exits: u32,
    }

    pub fn read(d: &[u8]) -> Result<ExitWindowFields, crate::LayoutError> {
        if d.len() < LEN {
            return Err(crate::LayoutError::TooShort {
                need: LEN,
                got: d.len(),
            });
        }
        if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
            return Err(crate::LayoutError::BadMagic);
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        Ok(ExitWindowFields {
            chain_id: u64_at(OFF_CHAIN_ID),
            window_index: u64_at(OFF_WINDOW_INDEX),
            spent_cap_units: u64_at(OFF_SPENT_CAP_UNITS),
            exits: u32_at(OFF_EXITS),
        })
    }

    pub fn write(f: &ExitWindowFields) -> [u8; LEN] {
        let mut d = [0u8; LEN];
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&f.chain_id.to_le_bytes());
        d[OFF_WINDOW_INDEX..OFF_WINDOW_INDEX + 8].copy_from_slice(&f.window_index.to_le_bytes());
        d[OFF_SPENT_CAP_UNITS..OFF_SPENT_CAP_UNITS + 8]
            .copy_from_slice(&f.spent_cap_units.to_le_bytes());
        d[OFF_EXITS..OFF_EXITS + 4].copy_from_slice(&f.exits.to_le_bytes());
        // OFF_PAD..OFF_PAD+4 stays zeroed — reserved, not part of ExitWindowFields.
        d
    }
}

/// The `exit_nullifier` account (`["exit_nullifier", chain_id, page]`, magic `"ZKEN"`, `LEN` 1,044) — a
/// persistent, never-recycled 1,024-byte bitmap (8,192 exits/page) of which nonces on this
/// page have ever been proved. `page = nonce >> 13`, via [`super::nullifier_page`].
pub mod exit_nullifier {
    pub const MAGIC: u32 = 0x5a4b_454e; // "ZKEN"

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_CHAIN_ID: usize = 4;
    pub const OFF_PAGE: usize = 12;
    pub const OFF_BITS: usize = 20;
    /// 8,192 exits/page ÷ 8 bits/byte.
    pub const BITS_LEN: usize = 1024;
    /// Full fixed-size account length.
    pub const LEN: usize = OFF_BITS + BITS_LEN;

    /// `["exit_nullifier", chain_id, page]`.
    #[inline]
    pub fn seeds(chain_id: u64, page: u64) -> [Vec<u8>; 3] {
        [
            b"exit_nullifier".to_vec(),
            chain_id.to_le_bytes().to_vec(),
            page.to_le_bytes().to_vec(),
        ]
    }

    /// Derives the `exit_nullifier` PDA under `program_id` (the settlement program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        chain_id: u64,
        page: u64,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(chain_id, page);
        solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NullifierHeader {
        pub chain_id: u64,
        pub page: u64,
    }

    /// Validates magic + minimum length and decodes the header (`chain_id`, `page`) — not the bitmap;
    /// callers slice `&d[OFF_BITS..]` themselves and pass it to [`super::bit_is_set`]/[`super::set_bit`].
    pub fn read_header(d: &[u8]) -> Result<NullifierHeader, crate::LayoutError> {
        if d.len() < LEN {
            return Err(crate::LayoutError::TooShort {
                need: LEN,
                got: d.len(),
            });
        }
        if u32::from_le_bytes(d[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
            return Err(crate::LayoutError::BadMagic);
        }
        Ok(NullifierHeader {
            chain_id: u64::from_le_bytes(d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].try_into().unwrap()),
            page: u64::from_le_bytes(d[OFF_PAGE..OFF_PAGE + 8].try_into().unwrap()),
        })
    }

    /// Writes the header (`magic`, `chain_id`, `page`) into `d` (must be at least [`LEN`] bytes) without
    /// touching the bitmap region — the one-time initialization a create-or-adopt path performs; every
    /// later touch only ever calls [`super::set_bit`] on the bits region.
    pub fn write_header(d: &mut [u8], chain_id: u64, page: u64) {
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_CHAIN_ID..OFF_CHAIN_ID + 8].copy_from_slice(&chain_id.to_le_bytes());
        d[OFF_PAGE..OFF_PAGE + 8].copy_from_slice(&page.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ExitMessage: pinned against the Solidity fixture ---

    fn hex_bytes(s: &str) -> Vec<u8> {
        hex::decode(s.trim_start_matches("0x")).unwrap()
    }

    fn hex32(s: &str) -> [u8; 32] {
        hex_bytes(s).try_into().unwrap()
    }

    fn hex20(s: &str) -> [u8; 20] {
        hex_bytes(s).try_into().unwrap()
    }

    /// The fixture's message, hand-copied from `fixtures/exit/message_hash.json` (not re-read from the
    /// file, so this test also pins the fixture's own numbers — a fixture regeneration that silently
    /// changed a field would go red here, not only in `rome-zk-mpt`'s fixture-reading tests).
    fn fixture_message() -> ExitMessage {
        ExitMessage {
            nonce: 0,
            l2_sender: hex20("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            sol_recipient: hex32(
                "0x0101010101010101010101010101010101010101010101010101010101010101",
            ),
            asset: [0u8; 20],
            amount: 1_000_000_000_000_000_000u128,
        }
    }

    #[test]
    fn preimage_is_160_bytes_and_matches_fixture() {
        let expected = hex_bytes(
            "0x0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266010101010101010101010101010101010101010101010101010101010101010100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000de0b6b3a7640000",
        );
        assert_eq!(expected.len(), 160);
        let preimage = fixture_message().message_preimage();
        assert_eq!(preimage.to_vec(), expected);
    }

    #[test]
    fn message_hash_matches_solidity_fixture() {
        let expected = hex32("0xf7ee5f200e860ecd5e2d5f00b7cad21dca1f5787ec01987c17e8e4766d44d92f");
        assert_eq!(fixture_message().message_hash(), expected);
    }

    #[test]
    fn storage_slot_matches_solidity_fixture() {
        let expected = hex32("0x766fe10d2f115c352e4db00af367fc73383e15c355b11a143a28f2984c8fda2f");
        assert_eq!(fixture_message().storage_slot(), expected);
    }

    // --- Cap units ---

    #[test]
    fn cap_units_rounds_up_and_refuses_overflow() {
        assert_eq!(cap_units(0).unwrap(), 0);
        assert_eq!(cap_units(1).unwrap(), 1); // sub-gwei remainder still costs one unit
        assert_eq!(cap_units(CAP_UNIT_WEI).unwrap(), 1); // exact gwei
        assert_eq!(cap_units(CAP_UNIT_WEI + 1).unwrap(), 2); // one wei over -> rounds up again
        assert_eq!(cap_units(CAP_UNIT_WEI * 20).unwrap(), 20);

        let max_expressible = CAP_UNIT_WEI * u64::MAX as u128;
        assert_eq!(cap_units(max_expressible).unwrap(), u64::MAX);
        assert_eq!(
            cap_units(max_expressible + 1).unwrap_err(),
            ExitAmountOverflow
        );
        assert_eq!(cap_units(u128::MAX).unwrap_err(), ExitAmountOverflow);
    }

    // --- Nullifier page/bit math ---

    #[test]
    fn nullifier_page_and_bit_math() {
        assert_eq!(nullifier_page(0), 0);
        assert_eq!(nullifier_bit(0), 0);

        assert_eq!(nullifier_page(8191), 0);
        assert_eq!(nullifier_bit(8191), 8191);

        assert_eq!(nullifier_page(8192), 1);
        assert_eq!(nullifier_bit(8192), 0);

        assert_eq!(nullifier_page(u64::MAX), u64::MAX >> 13);
        assert_eq!(nullifier_bit(u64::MAX), 8191);
    }

    #[test]
    fn bit_is_set_and_set_bit_round_trip_and_refuse_page_mismatch() {
        let mut bits = [0u8; exit_nullifier::BITS_LEN];
        let nonce = 8192 + 5; // page 1, bit 5
        assert!(!bit_is_set(&bits, 1, nonce).unwrap());
        set_bit(&mut bits, 1, nonce).unwrap();
        assert!(bit_is_set(&bits, 1, nonce).unwrap());
        // an unrelated bit on the same page is untouched
        assert!(!bit_is_set(&bits, 1, 8192).unwrap());

        assert_eq!(
            bit_is_set(&bits, 0, nonce).unwrap_err(),
            NullifierPageMismatch
        );
        assert_eq!(
            set_bit(&mut bits, 0, nonce).unwrap_err(),
            NullifierPageMismatch
        );
    }

    // --- Layout offsets/lengths pinned literally (never via the `OFF_` and `LEN` consts
    // themselves, so a mutated constant cannot mutate its own check) ---

    #[test]
    fn layout_offsets_and_lens_pinned() {
        assert_eq!(exit_config::OFF_MAGIC, 0);
        assert_eq!(exit_config::OFF_VERSION, 4);
        assert_eq!(exit_config::OFF_CHAIN_ID, 5);
        assert_eq!(exit_config::OFF_EXIT_PORTAL, 13);
        assert_eq!(exit_config::OFF_BRIDGE_PROGRAM, 33);
        assert_eq!(exit_config::OFF_PENDING_EXIT_PORTAL, 65);
        assert_eq!(exit_config::OFF_PENDING_BRIDGE_PROGRAM, 85);
        assert_eq!(exit_config::OFF_PENDING_EXIT_CAP, 117);
        assert_eq!(exit_config::OFF_PENDING_POSTER_BOND, 125);
        assert_eq!(exit_config::OFF_ACTIVATION_SLOT, 133);
        assert_eq!(exit_config::OFF_PENDING_MASK, 141);
        assert_eq!(exit_config::LEN, 142);

        assert_eq!(exit_record::OFF_MAGIC, 0);
        assert_eq!(exit_record::OFF_CHAIN_ID, 4);
        assert_eq!(exit_record::OFF_BATCH, 12);
        assert_eq!(exit_record::OFF_MESSAGE_HASH, 20);
        assert_eq!(exit_record::OFF_SOL_RECIPIENT, 52);
        assert_eq!(exit_record::OFF_AMOUNT, 84);
        assert_eq!(exit_record::OFF_WINDOW_INDEX, 100);
        assert_eq!(exit_record::OFF_PROVED_SLOT, 108);
        assert_eq!(exit_record::OFF_STATUS, 116);
        assert_eq!(exit_record::OFF_PAYER, 117);
        assert_eq!(exit_record::OFF_ASSET, 149);
        assert_eq!(exit_record::LEN, 169);

        assert_eq!(exit_window::OFF_MAGIC, 0);
        assert_eq!(exit_window::OFF_CHAIN_ID, 4);
        assert_eq!(exit_window::OFF_WINDOW_INDEX, 12);
        assert_eq!(exit_window::OFF_SPENT_CAP_UNITS, 20);
        assert_eq!(exit_window::OFF_EXITS, 28);
        assert_eq!(exit_window::OFF_PAD, 32);
        assert_eq!(exit_window::LEN, 36);

        assert_eq!(exit_nullifier::OFF_MAGIC, 0);
        assert_eq!(exit_nullifier::OFF_CHAIN_ID, 4);
        assert_eq!(exit_nullifier::OFF_PAGE, 12);
        assert_eq!(exit_nullifier::OFF_BITS, 20);
        assert_eq!(exit_nullifier::BITS_LEN, 1024);
        assert_eq!(exit_nullifier::LEN, 1044);
    }

    // --- read() refusals by name, one per layout ---

    #[test]
    fn read_refuses_bad_magic_version_len() {
        // exit_config: BadMagic, BadVersion, TooShort.
        let good = exit_config::write(&exit_config::ExitConfigFields {
            chain_id: 1,
            exit_portal: [1u8; 20],
            bridge_program: [2u8; 32],
            pending_exit_portal: [0u8; 20],
            pending_bridge_program: [0u8; 32],
            pending_exit_cap: 0,
            pending_poster_bond: 0,
            activation_slot: 0,
            pending_mask: 0,
        });
        let mut bad_magic = good;
        bad_magic[exit_config::OFF_MAGIC] ^= 0xff;
        assert_eq!(
            exit_config::read(&bad_magic).unwrap_err(),
            crate::LayoutError::BadMagic
        );
        let mut bad_version = good;
        bad_version[exit_config::OFF_VERSION] = 0xee;
        assert_eq!(
            exit_config::read(&bad_version).unwrap_err(),
            crate::LayoutError::BadVersion
        );
        assert!(matches!(
            exit_config::read(&good[..good.len() - 1]).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));

        // exit_record: BadMagic, TooShort (no version byte in this layout).
        let good = exit_record::write(&exit_record::ExitRecordFields {
            chain_id: 1,
            batch: 1,
            message_hash: [0u8; 32],
            sol_recipient: [0u8; 32],
            amount: 0,
            window_index: 0,
            proved_slot: 0,
            status: exit_record::STATUS_PROVED,
            payer: [0u8; 32],
            asset: [0u8; 20],
        });
        let mut bad_magic = good;
        bad_magic[exit_record::OFF_MAGIC] ^= 0xff;
        assert_eq!(
            exit_record::read(&bad_magic).unwrap_err(),
            crate::LayoutError::BadMagic
        );
        assert!(matches!(
            exit_record::read(&good[..good.len() - 1]).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));

        // exit_window: BadMagic, TooShort.
        let good = exit_window::write(&exit_window::ExitWindowFields {
            chain_id: 1,
            window_index: 0,
            spent_cap_units: 0,
            exits: 0,
        });
        let mut bad_magic = good;
        bad_magic[exit_window::OFF_MAGIC] ^= 0xff;
        assert_eq!(
            exit_window::read(&bad_magic).unwrap_err(),
            crate::LayoutError::BadMagic
        );
        assert!(matches!(
            exit_window::read(&good[..good.len() - 1]).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));

        // exit_nullifier: BadMagic, TooShort.
        let mut good = vec![0u8; exit_nullifier::LEN];
        exit_nullifier::write_header(&mut good, 1, 0);
        let mut bad_magic = good.clone();
        bad_magic[exit_nullifier::OFF_MAGIC] ^= 0xff;
        assert_eq!(
            exit_nullifier::read_header(&bad_magic).unwrap_err(),
            crate::LayoutError::BadMagic
        );
        assert!(matches!(
            exit_nullifier::read_header(&good[..good.len() - 1]).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }

    // --- round trips ---

    #[test]
    fn exit_config_round_trips() {
        let f = exit_config::ExitConfigFields {
            chain_id: 200101,
            exit_portal: [0x11u8; 20],
            bridge_program: [0x22u8; 32],
            pending_exit_portal: [0x33u8; 20],
            pending_bridge_program: [0x44u8; 32],
            pending_exit_cap: 1_000_000_000_000,
            pending_poster_bond: 2_000_000_000,
            activation_slot: 123_456,
            pending_mask: exit_config::PENDING_MASK_PORTAL | exit_config::PENDING_MASK_CAP,
        };
        let d = exit_config::write(&f);
        assert_eq!(d.len(), exit_config::LEN);
        assert_eq!(exit_config::read(&d).unwrap(), f);
    }

    #[test]
    fn exit_record_round_trips() {
        let f = exit_record::ExitRecordFields {
            chain_id: 200101,
            batch: 42,
            message_hash: [0x55u8; 32],
            sol_recipient: [0x66u8; 32],
            amount: 1_000_000_000_000_000_000u128,
            window_index: 7,
            proved_slot: 999,
            status: exit_record::STATUS_PROVED,
            payer: [0x77u8; 32],
            asset: [0u8; 20],
        };
        let d = exit_record::write(&f);
        assert_eq!(d.len(), exit_record::LEN);
        assert_eq!(exit_record::read(&d).unwrap(), f);
    }

    #[test]
    fn exit_window_round_trips() {
        let f = exit_window::ExitWindowFields {
            chain_id: 200101,
            window_index: 3,
            spent_cap_units: 500,
            exits: 4,
        };
        let d = exit_window::write(&f);
        assert_eq!(d.len(), exit_window::LEN);
        assert_eq!(exit_window::read(&d).unwrap(), f);
    }

    #[test]
    fn exit_nullifier_header_round_trips() {
        let mut d = vec![0u8; exit_nullifier::LEN];
        exit_nullifier::write_header(&mut d, 200101, 3);
        let hdr = exit_nullifier::read_header(&d).unwrap();
        assert_eq!(hdr.chain_id, 200101);
        assert_eq!(hdr.page, 3);
    }

    // --- PDA seed golden bytes (byte order/tag pinned independently of `pda()` itself) ---

    #[test]
    fn seeds_golden_bytes() {
        let s = exit_config::seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"exit_config".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);

        let s = exit_record::seeds(0x0102_0304_0506_0708, [0xabu8; 32]);
        assert_eq!(s[0], b"exit".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[2], vec![0xabu8; 32]);

        let s = exit_window::seeds(0x0102_0304_0506_0708, 9);
        assert_eq!(s[0], b"exit_window".to_vec());
        assert_eq!(s[2], vec![9, 0, 0, 0, 0, 0, 0, 0]);

        let s = exit_nullifier::seeds(0x0102_0304_0506_0708, 3);
        assert_eq!(s[0], b"exit_nullifier".to_vec());
        assert_eq!(s[2], vec![3, 0, 0, 0, 0, 0, 0, 0]);

        let s = exit_consumer_seeds(0x0102_0304_0506_0708);
        assert_eq!(s[0], b"exit_consumer".to_vec());
        assert_eq!(s[1], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    // --- exit_consumer PDA: the one derivation `ConsumeExit` and the later
    // `zk-bridge` program must agree on byte-for-byte ---

    #[cfg(feature = "solana")]
    #[test]
    fn exit_consumer_pda_is_deterministic_and_varies_with_chain_id_and_bridge_program() {
        let bridge_a = solana_program::pubkey::Pubkey::new_unique();
        let bridge_b = solana_program::pubkey::Pubkey::new_unique();

        let (a1, _) = exit_consumer_pda(7, &bridge_a);
        let (a2, _) = exit_consumer_pda(7, &bridge_a);
        assert_eq!(a1, a2, "same inputs must derive the same PDA");

        let (b, _) = exit_consumer_pda(8, &bridge_a);
        assert_ne!(a1, b, "a different chain_id must derive a different PDA");

        // The whole `NotBridgeProgram` guard rests on this: the SAME chain_id under a DIFFERENT program
        // derives a DIFFERENT PDA, so a program that is not the registered bridge cannot sign the
        // registered bridge's `exit_consumer` address no matter what seeds it tries.
        let (under_a, _) = exit_consumer_pda(7, &bridge_a);
        let (under_b, _) = exit_consumer_pda(7, &bridge_b);
        assert_ne!(
            under_a, under_b,
            "the exit_consumer PDA must be bound to the specific bridge program, not just the chain_id"
        );
    }

    #[cfg(feature = "solana")]
    #[test]
    fn pdas_are_deterministic_and_vary_with_their_own_inputs() {
        let program = solana_program::pubkey::Pubkey::new_unique();
        let (a1, _) = exit_config::pda(&program, 7);
        let (a2, _) = exit_config::pda(&program, 7);
        assert_eq!(a1, a2);
        let (b, _) = exit_config::pda(&program, 8);
        assert_ne!(a1, b);

        let (r1, _) = exit_record::pda(&program, 7, [1u8; 32]);
        let (r2, _) = exit_record::pda(&program, 7, [2u8; 32]);
        assert_ne!(r1, r2);

        let (w1, _) = exit_window::pda(&program, 7, 0);
        let (w2, _) = exit_window::pda(&program, 7, 1);
        assert_ne!(w1, w2);

        let (n1, _) = exit_nullifier::pda(&program, 7, 0);
        let (n2, _) = exit_nullifier::pda(&program, 7, 1);
        assert_ne!(n1, n2);
    }
}
