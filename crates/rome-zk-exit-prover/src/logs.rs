//! Decodes one `eth_getLogs` `ExitInitiated` entry ([`crate::rpc::LogEntry`]) into an
//! [`rome_zk_layouts::exit::ExitMessage`] — the ABI shape `contracts/exit-portal/src/RomeExitPortal.sol`
//! emits: `event ExitInitiated(bytes32 indexed messageHash, uint256 nonce, address l2Sender, bytes32
//! solRecipient, address asset, uint256 amount)`. `messageHash` is the log's one indexed field
//! (`topics[1]`, `topics[0]` being the event selector); the other five fields are the log's `data`, ABI-
//! encoded in declaration order — five 32-byte words, each address left-padded to 32 bytes.

use rome_zk_layouts::exit::ExitMessage;

use crate::rpc::LogEntry;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LogDecodeError {
    #[error("ExitInitiated log carries {got} topics, expected 2 (selector + indexed messageHash)")]
    WrongTopicCount { got: usize },
    #[error("topics[1] (messageHash) is not 32 bytes: {0}")]
    BadMessageHashHex(String),
    #[error("log data is {got} bytes, expected 160 (5 ABI words)")]
    WrongDataLen { got: usize },
    #[error("log data is not valid hex: {0}")]
    BadDataHex(String),
    #[error("the log's own messageHash (topics[1]) does not match keccak(abi.encode(the log's own fields)): computed {computed}, log says {logged}")]
    MessageHashMismatch { computed: String, logged: String },
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(strip_0x(s)).map_err(|e| e.to_string())?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("{} bytes, expected 32", v.len()))
}

fn addr_from_word(word: &[u8; 32]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(&word[12..32]);
    out
}

/// Decodes `log` into `(ExitMessage, log_index, block_number)` — the caller's own follow-loop cursor
/// state (`from_block` for the next `eth_getLogs` call) is derived from `block_number`, never stored
/// here.
pub fn decode_exit_initiated(log: &LogEntry) -> Result<(ExitMessage, u64, u64), LogDecodeError> {
    if log.topics.len() != 2 {
        return Err(LogDecodeError::WrongTopicCount {
            got: log.topics.len(),
        });
    }
    let message_hash = hex32(&log.topics[1])
        .map_err(|_| LogDecodeError::BadMessageHashHex(log.topics[1].clone()))?;

    let data =
        hex::decode(strip_0x(&log.data)).map_err(|e| LogDecodeError::BadDataHex(e.to_string()))?;
    if data.len() != 160 {
        return Err(LogDecodeError::WrongDataLen { got: data.len() });
    }
    let word = |i: usize| -> [u8; 32] {
        let mut w = [0u8; 32];
        w.copy_from_slice(&data[i * 32..(i + 1) * 32]);
        w
    };
    let nonce_word = word(0);
    let mut nonce_be8 = [0u8; 8];
    nonce_be8.copy_from_slice(&nonce_word[24..32]);
    let nonce = u64::from_be_bytes(nonce_be8);
    let l2_sender = addr_from_word(&word(1));
    let sol_recipient = word(2);
    let asset = addr_from_word(&word(3));
    let amount_word = word(4);
    let amount = u128::from_be_bytes(amount_word[16..32].try_into().unwrap());

    let message = ExitMessage {
        nonce,
        l2_sender,
        sol_recipient,
        asset,
        amount,
    };

    let computed = message.message_hash();
    if computed != message_hash {
        return Err(LogDecodeError::MessageHashMismatch {
            computed: format!("0x{}", hex::encode(computed)),
            logged: format!("0x{}", hex::encode(message_hash)),
        });
    }

    let block_number = u64::from_str_radix(strip_0x(&log.block_number), 16)
        .map_err(|_| LogDecodeError::BadDataHex(log.block_number.clone()))?;
    let log_index = u64::from_str_radix(strip_0x(&log.log_index), 16)
        .map_err(|_| LogDecodeError::BadDataHex(log.log_index.clone()))?;

    Ok((message, log_index, block_number))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANVIL_EXIT_LOGS: &str = include_str!("../../../fixtures/exit/anvil_exit_logs.json");
    const MESSAGE_HASH_JSON: &str = include_str!("../../../fixtures/exit/message_hash.json");

    #[derive(serde::Deserialize)]
    struct MessageHashFixture {
        message_hash: String,
    }

    #[test]
    fn logs_decode_the_exit_initiated_event() {
        let logs: Vec<LogEntry> = serde_json::from_str(ANVIL_EXIT_LOGS).unwrap();
        assert_eq!(logs.len(), 1);
        let (message, log_index, block_number) = decode_exit_initiated(&logs[0]).unwrap();

        let fixture: MessageHashFixture = serde_json::from_str(MESSAGE_HASH_JSON).unwrap();
        let expected_hash = hex32(&fixture.message_hash).unwrap();
        assert_eq!(message.message_hash(), expected_hash);
        assert_eq!(message.nonce, 0);
        assert_eq!(log_index, 0);
        assert_eq!(block_number, 2);
    }

    #[test]
    fn wrong_topic_count_is_refused() {
        let mut log: Vec<LogEntry> = serde_json::from_str(ANVIL_EXIT_LOGS).unwrap();
        log[0].topics.push("0x00".to_string());
        let err = decode_exit_initiated(&log[0]).unwrap_err();
        assert_eq!(err, LogDecodeError::WrongTopicCount { got: 3 });
    }

    #[test]
    fn tampered_data_fails_the_hash_check() {
        let mut logs: Vec<LogEntry> = serde_json::from_str(ANVIL_EXIT_LOGS).unwrap();
        // Flip the last hex digit of the amount word (the log's last 32-byte word) — the hash pinned in
        // topics[1] no longer matches what the (tampered) data would hash to.
        let mut chars: Vec<char> = logs[0].data.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        logs[0].data = chars.into_iter().collect();
        assert!(matches!(
            decode_exit_initiated(&logs[0]),
            Err(LogDecodeError::MessageHashMismatch { .. })
        ));
    }
}
