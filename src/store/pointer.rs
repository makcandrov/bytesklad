/// Number of low bits of a [`Pointer`] holding the logical offset.
const OFFSET_BITS: u32 = 56;

/// Largest addressable logical offset within a single bucket (64 PiB).
pub(crate) const MAX_OFFSET: u64 = (1 << OFFSET_BITS) - 1;

/// Size of a bucket's address space: offsets run from 0 to `MAX_OFFSET` inclusive.
pub(crate) const MAX_BUCKET_LEN: u64 = MAX_OFFSET + 1;

/// Tag `0` is reserved for inline values and tag `255` for the
/// variable-length bucket, leaving tags `1..=254` for size buckets.
pub(crate) const MAX_BUCKETS: usize = u8::MAX as usize - 1;

/// Tag of an inline value, which has no bucket and no file behind it: the
/// pointer alone carries the whole record.
pub(crate) const INLINE_TAG: u8 = 0;

/// Tag of the variable-length bucket.
pub(crate) const UNSIZED_TAG: u8 = u8::MAX;

/// Byte of a [`Pointer`] holding an inline value's length; the six below it
/// hold its payload, and the one above is the tag.
const LEN_BYTE: usize = 6;

/// Longest value that fits inline.
///
/// A pointer has 56 bits under its tag, and a seventh payload byte would fill
/// them exactly, leaving nothing to encode the length with. No cleverer
/// encoding rescues it either: the byte strings of length `0..=7` outnumber
/// the 56-bit patterns by a factor of `256/255`.
pub(crate) const MAX_INLINE_LEN: usize = LEN_BYTE;

/// The entire index value for one record, packed into eight bytes.
///
/// Under a bucket tag it is an 8-bit tag and a 56-bit logical offset inside
/// that bucket; the tag identifies the bucket, and for a size bucket it also
/// *is* the record's length — which is why the length is not stored.
///
/// ```txt
///    63       56 55                                              0
///   ┌───────────┬─────────────────────────────────────────────────┐
///   │  tag ≠ 0  │                   offset (56)                   │
///   └───────────┴─────────────────────────────────────────────────┘
/// ```
///
/// Under tag `0` those 56 bits hold the record itself: up to six payload
/// bytes and the length that says how many of them count.
///
/// ```txt
///    63       56 55     51 50  48 47                             0
///   ┌───────────┬─────────┬──────┬───────────────────────────────┐
///   │  tag = 0  │ zero (5)│ len  │        payload (48)           │
///   └───────────┴─────────┴──────┴───────────────────────────────┘
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pointer(u64);

impl Pointer {
    pub fn new(tag: u8, offset: u64) -> Option<Self> {
        (offset <= MAX_OFFSET).then(|| Self((u64::from(tag) << OFFSET_BITS) | offset))
    }

    /// Pack `value` into the pointer itself, or `None` if it is longer than
    /// [`MAX_INLINE_LEN`]. The empty value packs to the all-zero pointer.
    pub fn inline(value: &[u8]) -> Option<Self> {
        if value.len() > MAX_INLINE_LEN {
            return None;
        }
        let mut bytes = [0u8; 8];
        bytes[..value.len()].copy_from_slice(value);
        bytes[LEN_BYTE] = value.len() as u8;
        Some(Self(u64::from_le_bytes(bytes)))
    }

    /// The record an inline pointer carries, or `None` if its length field is
    /// out of range. Only meaningful when [`tag`](Self::tag) is [`INLINE_TAG`].
    pub fn inline_value(self) -> Option<Vec<u8>> {
        let bytes = self.0.to_le_bytes();
        let len = usize::from(bytes[LEN_BYTE]);
        (len <= MAX_INLINE_LEN).then(|| bytes[..len].to_vec())
    }

    pub fn tag(self) -> u8 {
        (self.0 >> OFFSET_BITS) as u8
    }

    pub fn offset(self) -> u64 {
        self.0 & MAX_OFFSET
    }

    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub fn from_le_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for (tag, offset) in [(1u8, 4096u64), (254, MAX_OFFSET), (7, 1 << 40)] {
            let ptr = Pointer::new(tag, offset).unwrap();
            assert_eq!(ptr.tag(), tag);
            assert_eq!(ptr.offset(), offset);
            assert_eq!(Pointer::from_le_bytes(ptr.to_le_bytes()), ptr);
        }
    }

    #[test]
    fn rejects_out_of_range_offset() {
        assert!(Pointer::new(1, MAX_OFFSET + 1).is_none());
    }

    #[test]
    fn inline_round_trips() {
        for value in [
            b"".as_slice(),
            b"\0",
            b"a",
            b"ab",
            b"abcdef",
            &[0xff; MAX_INLINE_LEN],
            &[0x00, 0xff, 0x00, 0xff, 0x00, 0xff],
        ] {
            let ptr = Pointer::inline(value).unwrap();
            assert_eq!(ptr.tag(), INLINE_TAG);
            assert_eq!(ptr.inline_value().unwrap(), value);
            assert_eq!(
                Pointer::from_le_bytes(ptr.to_le_bytes())
                    .inline_value()
                    .unwrap(),
                value
            );
        }
    }

    #[test]
    fn rejects_values_past_the_inline_limit() {
        assert!(Pointer::inline(&[0u8; MAX_INLINE_LEN + 1]).is_none());
    }

    #[test]
    fn the_empty_value_is_the_all_zero_pointer() {
        assert_eq!(
            Pointer::inline(b"").unwrap(),
            Pointer::from_le_bytes([0; 8])
        );
    }

    /// Distinct short values must never collide, which is what the explicit
    /// length field buys over trailing-zero trimming.
    #[test]
    fn trailing_zeros_are_not_significant_but_length_is() {
        assert_ne!(Pointer::inline(b"a"), Pointer::inline(b"a\0"));
        assert_ne!(Pointer::inline(b""), Pointer::inline(b"\0"));
    }

    #[test]
    fn an_out_of_range_length_field_is_rejected() {
        let mut bytes = [0u8; 8];
        bytes[LEN_BYTE] = (MAX_INLINE_LEN + 1) as u8;
        assert!(Pointer::from_le_bytes(bytes).inline_value().is_none());
    }
}
