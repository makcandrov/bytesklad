//! LEB128, used to frame records in the variable-length bucket.

pub(crate) const MAX_ENCODED_LEN: usize = 10;

pub(crate) fn encode(mut value: u64, buf: &mut [u8; MAX_ENCODED_LEN]) -> usize {
    let mut len = 0;
    loop {
        let byte = (value as u8) & 0x7f;
        value >>= 7;
        if value == 0 {
            buf[len] = byte;
            return len + 1;
        }
        buf[len] = byte | 0x80;
        len += 1;
    }
}

/// Returns the decoded value and the number of bytes consumed.
pub(crate) fn decode(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().take(MAX_ENCODED_LEN).enumerate() {
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut buf = [0u8; MAX_ENCODED_LEN];
        for value in [
            0,
            1,
            32,
            64,
            127,
            128,
            300,
            16_383,
            16_384,
            1 << 40,
            u64::MAX,
        ] {
            let len = encode(value, &mut buf);
            assert_eq!(decode(&buf[..len]), Some((value, len)));
        }
    }

    #[test]
    fn common_sizes_frame_in_one_byte() {
        let mut buf = [0u8; MAX_ENCODED_LEN];
        assert_eq!(encode(32, &mut buf), 1);
        assert_eq!(encode(64, &mut buf), 1);
    }

    #[test]
    fn truncated_input_fails() {
        let mut buf = [0u8; MAX_ENCODED_LEN];
        let len = encode(1 << 40, &mut buf);
        assert_eq!(decode(&buf[..len - 1]), None);
    }
}
