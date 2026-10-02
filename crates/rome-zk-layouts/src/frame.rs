//! The channel/frame header: a fixed 19-byte prefix on every frame the batcher cuts a
//! batch's compressed channel stream into (`rome-zk-batcher::channel::Frame`), and derivation decodes
//! back out (via `zk-inbox-client::decode_chunk_header`'s body, which is one frame's `to_bytes()`
//! encoding).
//!
//! ```text
//! channel_id: [u8; 16]   // keccak256(chain_id_le[8] ++ batch_le[8])[..16]
//! frame_no:   u16 (LE)
//! is_last:    u8         // 1 on the final frame of the channel, 0 otherwise
//! ```
//! All integers little-endian.

pub const OFF_CHANNEL_ID: usize = 0;
pub const OFF_FRAME_NO: usize = 16;
pub const OFF_IS_LAST: usize = 18;
/// Frame header length: 16-byte channel id + 2-byte frame number + 1-byte `is_last` flag.
pub const FRAME_HEADER_LEN: usize = 19;

/// Field-for-field decode of a frame header (not the body that follows it — the caller slices
/// `d[FRAME_HEADER_LEN..]` itself, since the body length is only known from the surrounding chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeaderFields {
    pub channel_id: [u8; 16],
    pub frame_no: u16,
    pub is_last: bool,
}

/// Validates minimum length and decodes every header field. No magic byte in this header
/// — `channel_id` itself is the only integrity check available, and it is checked by the caller against
/// the `(chain_id, batch)` it already expects, not by this function.
pub fn read(d: &[u8]) -> Result<FrameHeaderFields, crate::LayoutError> {
    if d.len() < FRAME_HEADER_LEN {
        return Err(crate::LayoutError::TooShort {
            need: FRAME_HEADER_LEN,
            got: d.len(),
        });
    }
    Ok(FrameHeaderFields {
        channel_id: d[OFF_CHANNEL_ID..OFF_CHANNEL_ID + 16].try_into().unwrap(),
        frame_no: u16::from_le_bytes(d[OFF_FRAME_NO..OFF_FRAME_NO + 2].try_into().unwrap()),
        is_last: d[OFF_IS_LAST] != 0,
    })
}

/// Encodes a frame header (the inverse of [`read`]). Returns exactly [`FRAME_HEADER_LEN`] bytes — the
/// caller appends the frame body.
pub fn write_header(f: &FrameHeaderFields) -> [u8; FRAME_HEADER_LEN] {
    let mut d = [0u8; FRAME_HEADER_LEN];
    d[OFF_CHANNEL_ID..OFF_CHANNEL_ID + 16].copy_from_slice(&f.channel_id);
    d[OFF_FRAME_NO..OFF_FRAME_NO + 2].copy_from_slice(&f.frame_no.to_le_bytes());
    d[OFF_IS_LAST] = f.is_last as u8;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-golden test (contract): a **literal** 19-byte vector (every byte position written as a
    /// number, never through `OFF_*`) decodes to the exact fields it encodes — pins the field order,
    /// width and offsets independently of the constants `read` itself uses, so a future change to
    /// `FRAME_HEADER_LEN` or an `OFF_*` value is caught here even though `read` and `write_header` would
    /// still agree with each other.
    #[test]
    fn read_decodes_a_known_byte_vector() {
        #[rustfmt::skip]
        let d: [u8; 19] = [
            // channel_id: 16 bytes of 0x11
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            // frame_no = 7 LE u16
            7, 0,
            // is_last = 1
            1,
        ];
        let f = read(&d).unwrap();
        assert_eq!(
            f,
            FrameHeaderFields {
                channel_id: [0x11u8; 16],
                frame_no: 7,
                is_last: true,
            }
        );
    }

    #[test]
    fn write_header_then_read_round_trips() {
        let f = FrameHeaderFields {
            channel_id: [0x22u8; 16],
            frame_no: 300,
            is_last: false,
        };
        let d = write_header(&f);
        assert_eq!(d.len(), FRAME_HEADER_LEN);
        assert_eq!(read(&d).unwrap(), f);
    }

    #[test]
    fn read_rejects_too_short() {
        let d = vec![0u8; FRAME_HEADER_LEN - 1];
        assert!(matches!(
            read(&d).unwrap_err(),
            crate::LayoutError::TooShort { .. }
        ));
    }
}
