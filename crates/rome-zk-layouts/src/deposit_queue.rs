//! The zk-bridge deposit accounts: the one-time bridge config ("ZKBG"), a chain's deposit queue
//! ("ZKDQ") and one deposit record ("ZKDR"), plus the token-amount to gwei conversion. The bridge, the
//! inbox, the sequencer, the batcher, derive and prover-input all read these accounts through this
//! module and no other decoder.
//!
//! ```text
//! bridge_config   ["bridge_config"]                                 (69 B)
//!   magic 'ZKBG' u32 | version u8 | settlement_program [32] | inbox_program [32]
//!
//! deposit_queue   ["deposit_queue", settlement_program, chain_id]   (165 B)
//!   magic 'ZKDQ' u32 | version u8 | count u64 | head_hash [32]
//!   | inclusion_deadline_secs u32 | max_per_batch u16 | max_per_block u16 | min_amount u64
//!   | fee_lamports u64 | fee_recipient [32]
//!   | the same six parameters again (the pending proposal) | activation_slot u64
//!
//! deposit_record  ["deposit", settlement_program, chain_id, index]  (113 B)
//!   magic 'ZKDR' u32 | version u8 | index u64 | enqueue_unix_ts i64 | sender [32] | recipient [20]
//!   | amount_gwei u64 | hash_after [32]
//! ```
//! All integers little-endian; `chain_id` and `index` in the seeds are 8-byte little-endian. Every
//! account is owned by the bridge program, and `settlement_program` in the seeds is the chain's
//! settlement program. `activation_slot` 0 means no proposal is pending (the pending parameters are
//! then ignored by readers and written as zeros).

use crate::LayoutError;

fn check_header(d: &[u8], len: usize, magic: u32, version: u8) -> Result<(), LayoutError> {
    if d.len() < len {
        return Err(LayoutError::TooShort {
            need: len,
            got: d.len(),
        });
    }
    if u32::from_le_bytes(d[0..4].try_into().unwrap()) != magic {
        return Err(LayoutError::BadMagic);
    }
    if d[4] != version {
        return Err(LayoutError::BadVersion);
    }
    Ok(())
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}
fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn i64_at(d: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn b32_at(d: &[u8], o: usize) -> [u8; 32] {
    d[o..o + 32].try_into().unwrap()
}

/// The bridge's one-time config: the canonical settlement program and the canonical inbox.
pub mod bridge_config {
    use super::*;

    pub const MAGIC: u32 = 0x5a4b_4247; // "ZKBG"
    pub const VERSION: u8 = 1;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_SETTLEMENT_PROGRAM: usize = 5;
    pub const OFF_INBOX_PROGRAM: usize = 37;
    /// Full fixed-size account length.
    pub const LEN: usize = 69;

    /// `["bridge_config"]`, under the bridge program.
    #[inline]
    pub fn seeds() -> [&'static [u8]; 1] {
        [b"bridge_config"]
    }

    /// Derives the `bridge_config` PDA under `program_id` (the bridge program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        solana_program::pubkey::Pubkey::find_program_address(&seeds(), program_id)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BridgeConfigFields {
        pub settlement_program: [u8; 32],
        pub inbox_program: [u8; 32],
    }

    /// Validates magic, version and length, then decodes every field.
    pub fn read(d: &[u8]) -> Result<BridgeConfigFields, LayoutError> {
        check_header(d, LEN, MAGIC, VERSION)?;
        Ok(BridgeConfigFields {
            settlement_program: b32_at(d, OFF_SETTLEMENT_PROGRAM),
            inbox_program: b32_at(d, OFF_INBOX_PROGRAM),
        })
    }

    /// Writes magic, version and every field. `d` must be at least `LEN` bytes.
    pub fn write(d: &mut [u8], f: &BridgeConfigFields) {
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_SETTLEMENT_PROGRAM..OFF_SETTLEMENT_PROGRAM + 32]
            .copy_from_slice(&f.settlement_program);
        d[OFF_INBOX_PROGRAM..OFF_INBOX_PROGRAM + 32].copy_from_slice(&f.inbox_program);
    }
}

/// A chain's deposit queue: the running count and hash head, the live parameters, and one pending
/// proposal of the same parameters.
#[allow(clippy::module_inception)]
pub mod deposit_queue {
    use super::*;

    pub const MAGIC: u32 = 0x5a4b_4451; // "ZKDQ"
    pub const VERSION: u8 = 1;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_COUNT: usize = 5;
    pub const OFF_HEAD_HASH: usize = 13;
    /// Start of the live parameters block.
    pub const OFF_PARAMS: usize = 45;
    /// Start of the pending proposal's parameters block.
    pub const OFF_PENDING: usize = 101;
    pub const OFF_ACTIVATION_SLOT: usize = 157;
    /// Size of one parameters block.
    pub const PARAMS_LEN: usize = 56;
    /// Offsets inside a parameters block.
    pub const P_INCLUSION_DEADLINE_SECS: usize = 0;
    pub const P_MAX_PER_BATCH: usize = 4;
    pub const P_MAX_PER_BLOCK: usize = 6;
    pub const P_MIN_AMOUNT: usize = 8;
    pub const P_FEE_LAMPORTS: usize = 16;
    pub const P_FEE_RECIPIENT: usize = 24;
    /// Full fixed-size account length.
    pub const LEN: usize = 165;

    /// `["deposit_queue", settlement_program, chain_id]`, under the bridge program.
    #[inline]
    pub fn seeds(settlement_program: &[u8; 32], chain_id: u64) -> [Vec<u8>; 3] {
        [
            b"deposit_queue".to_vec(),
            settlement_program.to_vec(),
            chain_id.to_le_bytes().to_vec(),
        ]
    }

    /// Derives the `deposit_queue` PDA under `program_id` (the bridge program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        settlement_program: &[u8; 32],
        chain_id: u64,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(settlement_program, chain_id);
        solana_program::pubkey::Pubkey::find_program_address(&[&s[0], &s[1], &s[2]], program_id)
    }

    /// The six parameters a queue runs on; a pending proposal holds the same six.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DepositParams {
        pub inclusion_deadline_secs: u32,
        pub max_per_batch: u16,
        pub max_per_block: u16,
        pub min_amount: u64,
        pub fee_lamports: u64,
        pub fee_recipient: [u8; 32],
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DepositQueueFields {
        pub count: u64,
        pub head_hash: [u8; 32],
        pub params: DepositParams,
        pub pending: DepositParams,
        /// 0 means no proposal is pending.
        pub activation_slot: u64,
    }

    fn read_params(d: &[u8], base: usize) -> DepositParams {
        DepositParams {
            inclusion_deadline_secs: u32_at(d, base + P_INCLUSION_DEADLINE_SECS),
            max_per_batch: u16_at(d, base + P_MAX_PER_BATCH),
            max_per_block: u16_at(d, base + P_MAX_PER_BLOCK),
            min_amount: u64_at(d, base + P_MIN_AMOUNT),
            fee_lamports: u64_at(d, base + P_FEE_LAMPORTS),
            fee_recipient: b32_at(d, base + P_FEE_RECIPIENT),
        }
    }

    fn write_params(d: &mut [u8], base: usize, p: &DepositParams) {
        d[base + P_INCLUSION_DEADLINE_SECS..base + P_INCLUSION_DEADLINE_SECS + 4]
            .copy_from_slice(&p.inclusion_deadline_secs.to_le_bytes());
        d[base + P_MAX_PER_BATCH..base + P_MAX_PER_BATCH + 2]
            .copy_from_slice(&p.max_per_batch.to_le_bytes());
        d[base + P_MAX_PER_BLOCK..base + P_MAX_PER_BLOCK + 2]
            .copy_from_slice(&p.max_per_block.to_le_bytes());
        d[base + P_MIN_AMOUNT..base + P_MIN_AMOUNT + 8]
            .copy_from_slice(&p.min_amount.to_le_bytes());
        d[base + P_FEE_LAMPORTS..base + P_FEE_LAMPORTS + 8]
            .copy_from_slice(&p.fee_lamports.to_le_bytes());
        d[base + P_FEE_RECIPIENT..base + P_FEE_RECIPIENT + 32].copy_from_slice(&p.fee_recipient);
    }

    /// Validates magic, version and length, then decodes every field.
    pub fn read(d: &[u8]) -> Result<DepositQueueFields, LayoutError> {
        check_header(d, LEN, MAGIC, VERSION)?;
        Ok(DepositQueueFields {
            count: u64_at(d, OFF_COUNT),
            head_hash: b32_at(d, OFF_HEAD_HASH),
            params: read_params(d, OFF_PARAMS),
            pending: read_params(d, OFF_PENDING),
            activation_slot: u64_at(d, OFF_ACTIVATION_SLOT),
        })
    }

    /// Writes magic, version and every field. `d` must be at least `LEN` bytes.
    pub fn write(d: &mut [u8], f: &DepositQueueFields) {
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_COUNT..OFF_COUNT + 8].copy_from_slice(&f.count.to_le_bytes());
        d[OFF_HEAD_HASH..OFF_HEAD_HASH + 32].copy_from_slice(&f.head_hash);
        write_params(d, OFF_PARAMS, &f.params);
        write_params(d, OFF_PENDING, &f.pending);
        d[OFF_ACTIVATION_SLOT..OFF_ACTIVATION_SLOT + 8]
            .copy_from_slice(&f.activation_slot.to_le_bytes());
    }
}

/// One deposit record: what the depositor sent, and the hash chain's value after it.
pub mod deposit_record {
    use super::*;

    pub const MAGIC: u32 = 0x5a4b_4452; // "ZKDR"
    pub const VERSION: u8 = 1;

    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    pub const OFF_INDEX: usize = 5;
    pub const OFF_ENQUEUE_UNIX_TS: usize = 13;
    pub const OFF_SENDER: usize = 21;
    pub const OFF_RECIPIENT: usize = 53;
    pub const OFF_AMOUNT_GWEI: usize = 73;
    pub const OFF_HASH_AFTER: usize = 81;
    /// Full fixed-size account length.
    pub const LEN: usize = 113;

    /// `["deposit", settlement_program, chain_id, index]`, under the bridge program.
    #[inline]
    pub fn seeds(settlement_program: &[u8; 32], chain_id: u64, index: u64) -> [Vec<u8>; 4] {
        [
            b"deposit".to_vec(),
            settlement_program.to_vec(),
            chain_id.to_le_bytes().to_vec(),
            index.to_le_bytes().to_vec(),
        ]
    }

    /// Derives the `deposit_record` PDA under `program_id` (the bridge program).
    #[cfg(feature = "solana")]
    #[inline]
    pub fn pda(
        program_id: &solana_program::pubkey::Pubkey,
        settlement_program: &[u8; 32],
        chain_id: u64,
        index: u64,
    ) -> (solana_program::pubkey::Pubkey, u8) {
        let s = seeds(settlement_program, chain_id, index);
        solana_program::pubkey::Pubkey::find_program_address(
            &[&s[0], &s[1], &s[2], &s[3]],
            program_id,
        )
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DepositRecordFields {
        pub index: u64,
        pub enqueue_unix_ts: i64,
        pub sender: [u8; 32],
        pub recipient: [u8; 20],
        pub amount_gwei: u64,
        pub hash_after: [u8; 32],
    }

    /// Validates magic, version and length, then decodes every field.
    pub fn read(d: &[u8]) -> Result<DepositRecordFields, LayoutError> {
        check_header(d, LEN, MAGIC, VERSION)?;
        Ok(DepositRecordFields {
            index: u64_at(d, OFF_INDEX),
            enqueue_unix_ts: i64_at(d, OFF_ENQUEUE_UNIX_TS),
            sender: b32_at(d, OFF_SENDER),
            recipient: d[OFF_RECIPIENT..OFF_RECIPIENT + 20].try_into().unwrap(),
            amount_gwei: u64_at(d, OFF_AMOUNT_GWEI),
            hash_after: b32_at(d, OFF_HASH_AFTER),
        })
    }

    /// Writes magic, version and every field. `d` must be at least `LEN` bytes.
    pub fn write(d: &mut [u8], f: &DepositRecordFields) {
        d[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
        d[OFF_VERSION] = VERSION;
        d[OFF_INDEX..OFF_INDEX + 8].copy_from_slice(&f.index.to_le_bytes());
        d[OFF_ENQUEUE_UNIX_TS..OFF_ENQUEUE_UNIX_TS + 8]
            .copy_from_slice(&f.enqueue_unix_ts.to_le_bytes());
        d[OFF_SENDER..OFF_SENDER + 32].copy_from_slice(&f.sender);
        d[OFF_RECIPIENT..OFF_RECIPIENT + 20].copy_from_slice(&f.recipient);
        d[OFF_AMOUNT_GWEI..OFF_AMOUNT_GWEI + 8].copy_from_slice(&f.amount_gwei.to_le_bytes());
        d[OFF_HASH_AFTER..OFF_HASH_AFTER + 32].copy_from_slice(&f.hash_after);
    }
}

/// Why a token amount cannot be turned into gwei.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountError {
    /// The mint has more than 9 decimals, so a base unit is finer than one gwei.
    DecimalsAbove9,
    /// The result does not fit in a `u64`.
    Overflow,
}

/// Converts a token amount in base units to gwei: `amount * 10^(9 - decimals)`, checked. Refuses a
/// mint with more than 9 decimals.
pub fn amount_gwei(amount: u64, decimals: u8) -> Result<u64, AmountError> {
    if decimals > 9 {
        return Err(AmountError::DecimalsAbove9);
    }
    let scale = 10u64.pow(9 - decimals as u32);
    amount.checked_mul(scale).ok_or(AmountError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // ---- bridge_config ----

    fn bridge_sample() -> bridge_config::BridgeConfigFields {
        bridge_config::BridgeConfigFields {
            settlement_program: [0x11; 32],
            inbox_program: [0x22; 32],
        }
    }

    #[test]
    fn bridge_config_golden_bytes() {
        assert_eq!(bridge_config::LEN, 69);
        let mut want = hex("4742 4b5a 01");
        want.extend_from_slice(&[0x11; 32]);
        want.extend_from_slice(&[0x22; 32]);
        let mut d = vec![0u8; bridge_config::LEN];
        bridge_config::write(&mut d, &bridge_sample());
        assert_eq!(d, want);
        assert_eq!(bridge_config::read(&want).unwrap(), bridge_sample());
        assert_eq!(d[bridge_config::OFF_SETTLEMENT_PROGRAM], 0x11);
        assert_eq!(d[bridge_config::OFF_INBOX_PROGRAM], 0x22);
    }

    #[test]
    fn bridge_config_rejects_bad_input() {
        let mut d = vec![0u8; bridge_config::LEN];
        bridge_config::write(&mut d, &bridge_sample());
        assert!(matches!(
            bridge_config::read(&d[..bridge_config::LEN - 1]),
            Err(LayoutError::TooShort { need: 69, got: 68 })
        ));
        let mut bad = d.clone();
        bad[0] ^= 0xff;
        assert_eq!(bridge_config::read(&bad), Err(LayoutError::BadMagic));
        let mut bad = d.clone();
        bad[4] = 2;
        assert_eq!(bridge_config::read(&bad), Err(LayoutError::BadVersion));
    }

    #[test]
    fn bridge_config_seeds_golden() {
        assert_eq!(bridge_config::seeds(), [b"bridge_config".as_slice()]);
    }

    // ---- deposit_queue ----

    fn params(base: u8) -> deposit_queue::DepositParams {
        deposit_queue::DepositParams {
            inclusion_deadline_secs: 0x0102_0304 + base as u32,
            max_per_batch: 0x0506 + base as u16,
            max_per_block: 0x0708 + base as u16,
            min_amount: 0x1112_1314_1516_1718 + base as u64,
            fee_lamports: 0x2122_2324_2526_2728 + base as u64,
            fee_recipient: [0x30 + base; 32],
        }
    }

    fn queue_sample() -> deposit_queue::DepositQueueFields {
        deposit_queue::DepositQueueFields {
            count: 0x0a0b_0c0d_0e0f_1011,
            head_hash: [0x77; 32],
            params: params(0),
            pending: params(1),
            activation_slot: 0x4142_4344_4546_4748,
        }
    }

    #[test]
    fn deposit_queue_golden_bytes() {
        assert_eq!(deposit_queue::LEN, 165);
        let mut want = hex("5144 4b5a 01");
        want.extend_from_slice(&hex("1110 0f0e 0d0c 0b0a"));
        want.extend_from_slice(&[0x77; 32]);
        // live parameters
        want.extend_from_slice(&hex("0403 0201"));
        want.extend_from_slice(&hex("0605"));
        want.extend_from_slice(&hex("0807"));
        want.extend_from_slice(&hex("1817 1615 1413 1211"));
        want.extend_from_slice(&hex("2827 2625 2423 2221"));
        want.extend_from_slice(&[0x30; 32]);
        // pending parameters (each base value plus 1)
        want.extend_from_slice(&hex("0503 0201"));
        want.extend_from_slice(&hex("0705"));
        want.extend_from_slice(&hex("0907"));
        want.extend_from_slice(&hex("1917 1615 1413 1211"));
        want.extend_from_slice(&hex("2927 2625 2423 2221"));
        want.extend_from_slice(&[0x31; 32]);
        want.extend_from_slice(&hex("4847 4645 4443 4241"));
        assert_eq!(want.len(), deposit_queue::LEN);

        let mut d = vec![0u8; deposit_queue::LEN];
        deposit_queue::write(&mut d, &queue_sample());
        assert_eq!(d, want);
        assert_eq!(deposit_queue::read(&want).unwrap(), queue_sample());
    }

    #[test]
    fn deposit_queue_offsets() {
        use deposit_queue::*;
        assert_eq!(OFF_COUNT, 5);
        assert_eq!(OFF_HEAD_HASH, 13);
        assert_eq!(OFF_PARAMS, 45);
        assert_eq!(OFF_PARAMS + P_INCLUSION_DEADLINE_SECS, 45);
        assert_eq!(OFF_PARAMS + P_MAX_PER_BATCH, 49);
        assert_eq!(OFF_PARAMS + P_MAX_PER_BLOCK, 51);
        assert_eq!(OFF_PARAMS + P_MIN_AMOUNT, 53);
        assert_eq!(OFF_PARAMS + P_FEE_LAMPORTS, 61);
        assert_eq!(OFF_PARAMS + P_FEE_RECIPIENT, 69);
        assert_eq!(OFF_PARAMS + PARAMS_LEN, OFF_PENDING);
        assert_eq!(OFF_PENDING, 101);
        assert_eq!(OFF_PENDING + PARAMS_LEN, OFF_ACTIVATION_SLOT);
        assert_eq!(OFF_ACTIVATION_SLOT, 157);
        assert_eq!(OFF_ACTIVATION_SLOT + 8, LEN);
    }

    #[test]
    fn deposit_queue_no_pending_round_trips() {
        let mut f = queue_sample();
        f.pending = deposit_queue::DepositParams::default();
        f.activation_slot = 0;
        let mut d = vec![0u8; deposit_queue::LEN];
        deposit_queue::write(&mut d, &f);
        assert!(d[deposit_queue::OFF_PENDING..].iter().all(|b| *b == 0));
        assert_eq!(deposit_queue::read(&d).unwrap(), f);
    }

    #[test]
    fn deposit_queue_rejects_bad_input() {
        let mut d = vec![0u8; deposit_queue::LEN];
        deposit_queue::write(&mut d, &queue_sample());
        assert!(matches!(
            deposit_queue::read(&d[..deposit_queue::LEN - 1]),
            Err(LayoutError::TooShort {
                need: 165,
                got: 164
            })
        ));
        let mut bad = d.clone();
        bad[0] ^= 0xff;
        assert_eq!(deposit_queue::read(&bad), Err(LayoutError::BadMagic));
        let mut bad = d.clone();
        bad[4] = 2;
        assert_eq!(deposit_queue::read(&bad), Err(LayoutError::BadVersion));
    }

    #[test]
    fn deposit_queue_seeds_golden() {
        let s = deposit_queue::seeds(&[0xab; 32], 0x0102_0304_0506_0708);
        assert_eq!(s[0], b"deposit_queue".to_vec());
        assert_eq!(s[1], vec![0xab; 32]);
        assert_eq!(s[2], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    // ---- deposit_record ----

    fn record_sample() -> deposit_record::DepositRecordFields {
        deposit_record::DepositRecordFields {
            index: 0x0102_0304_0506_0708,
            enqueue_unix_ts: -2,
            sender: [0x55; 32],
            recipient: [0x66; 20],
            amount_gwei: 0x1122_3344_5566_7788,
            hash_after: [0x99; 32],
        }
    }

    #[test]
    fn deposit_record_golden_bytes() {
        assert_eq!(deposit_record::LEN, 113);
        let mut want = hex("5244 4b5a 01");
        want.extend_from_slice(&hex("0807 0605 0403 0201"));
        want.extend_from_slice(&hex("feff ffff ffff ffff"));
        want.extend_from_slice(&[0x55; 32]);
        want.extend_from_slice(&[0x66; 20]);
        want.extend_from_slice(&hex("8877 6655 4433 2211"));
        want.extend_from_slice(&[0x99; 32]);
        assert_eq!(want.len(), deposit_record::LEN);

        let mut d = vec![0u8; deposit_record::LEN];
        deposit_record::write(&mut d, &record_sample());
        assert_eq!(d, want);
        assert_eq!(deposit_record::read(&want).unwrap(), record_sample());
    }

    #[test]
    fn deposit_record_offsets() {
        use deposit_record::*;
        assert_eq!(OFF_INDEX, 5);
        assert_eq!(OFF_ENQUEUE_UNIX_TS, 13);
        assert_eq!(OFF_SENDER, 21);
        assert_eq!(OFF_RECIPIENT, 53);
        assert_eq!(OFF_AMOUNT_GWEI, 73);
        assert_eq!(OFF_HASH_AFTER, 81);
        assert_eq!(OFF_HASH_AFTER + 32, LEN);
    }

    #[test]
    fn deposit_record_rejects_bad_input() {
        let mut d = vec![0u8; deposit_record::LEN];
        deposit_record::write(&mut d, &record_sample());
        assert!(matches!(
            deposit_record::read(&d[..deposit_record::LEN - 1]),
            Err(LayoutError::TooShort {
                need: 113,
                got: 112
            })
        ));
        let mut bad = d.clone();
        bad[0] ^= 0xff;
        assert_eq!(deposit_record::read(&bad), Err(LayoutError::BadMagic));
        let mut bad = d.clone();
        bad[4] = 2;
        assert_eq!(deposit_record::read(&bad), Err(LayoutError::BadVersion));
    }

    #[test]
    fn deposit_record_seeds_golden() {
        let s = deposit_record::seeds(&[0xab; 32], 0x0102_0304_0506_0708, 3);
        assert_eq!(s[0], b"deposit".to_vec());
        assert_eq!(s[1], vec![0xab; 32]);
        assert_eq!(s[2], vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(s[3], vec![3, 0, 0, 0, 0, 0, 0, 0]);
    }

    // ---- PDAs ----

    #[cfg(feature = "solana")]
    #[test]
    fn pdas_follow_the_seeds_and_vary_with_every_input() {
        use solana_program::pubkey::Pubkey;
        let bridge = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let sp = [0x42u8; 32];

        let (cfg, _) = bridge_config::pda(&bridge);
        assert_eq!(
            cfg,
            Pubkey::find_program_address(&[b"bridge_config"], &bridge).0
        );
        assert_ne!(cfg, bridge_config::pda(&other).0);

        let (q, _) = deposit_queue::pda(&bridge, &sp, 7);
        assert_eq!(
            q,
            Pubkey::find_program_address(&[b"deposit_queue", &sp, &7u64.to_le_bytes()], &bridge).0
        );
        assert_ne!(q, deposit_queue::pda(&bridge, &sp, 8).0);
        assert_ne!(q, deposit_queue::pda(&bridge, &[0x43; 32], 7).0);
        assert_ne!(q, deposit_queue::pda(&other, &sp, 7).0);

        let (r, _) = deposit_record::pda(&bridge, &sp, 7, 0);
        assert_eq!(
            r,
            Pubkey::find_program_address(
                &[b"deposit", &sp, &7u64.to_le_bytes(), &0u64.to_le_bytes()],
                &bridge
            )
            .0
        );
        assert_ne!(r, deposit_record::pda(&bridge, &sp, 7, 1).0);
        assert_ne!(r, deposit_record::pda(&bridge, &sp, 8, 0).0);
        assert_ne!(r, q);
    }

    // ---- amount_gwei ----

    #[test]
    fn amount_gwei_conversions() {
        // d = 9: unchanged
        assert_eq!(amount_gwei(1_500_000_000, 9), Ok(1_500_000_000));
        // d = 6 (USDC): 1 USDC = 1_000_000 base units = 1_000_000_000 gwei
        assert_eq!(amount_gwei(1_000_000, 6), Ok(1_000_000_000));
        assert_eq!(amount_gwei(1, 6), Ok(1_000));
        // d = 0: times 10^9
        assert_eq!(amount_gwei(5, 0), Ok(5_000_000_000));
        assert_eq!(amount_gwei(0, 0), Ok(0));
        // d = 10: refused
        assert_eq!(amount_gwei(1, 10), Err(AmountError::DecimalsAbove9));
        assert_eq!(amount_gwei(1, 255), Err(AmountError::DecimalsAbove9));
        // overflow
        assert_eq!(amount_gwei(u64::MAX, 0), Err(AmountError::Overflow));
        assert_eq!(amount_gwei(u64::MAX, 8), Err(AmountError::Overflow));
        assert_eq!(amount_gwei(u64::MAX, 9), Ok(u64::MAX));
        // the exact boundary at d = 0
        let max_ok = u64::MAX / 1_000_000_000;
        assert_eq!(amount_gwei(max_ok, 0), Ok(max_ok * 1_000_000_000));
        assert_eq!(amount_gwei(max_ok + 1, 0), Err(AmountError::Overflow));
    }
}
