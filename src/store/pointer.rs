/// Number of low bits of a [`Pointer`] holding the logical offset.
const OFFSET_BITS: u32 = 56;

/// Largest addressable logical offset within a single bucket (64 PiB).
pub(crate) const MAX_OFFSET: u64 = (1 << OFFSET_BITS) - 1;

/// Tag `0` is reserved for the variable-length bucket, leaving tags `1..=255`
/// for size buckets.
pub(crate) const MAX_BUCKETS: usize = u8::MAX as usize;

/// Tag of the variable-length bucket.
pub(crate) const UNSIZED_TAG: u8 = 0;

/// The entire index value for one record, packed into eight bytes:
/// an 8-bit bucket tag and a 56-bit logical offset inside that bucket.
///
/// The tag identifies which bucket holds the record, and for a size bucket it
/// also *is* the record's length — which is why the length is not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pointer(u64);

impl Pointer {
    pub fn new(tag: u8, offset: u64) -> Option<Self> {
        (offset <= MAX_OFFSET).then(|| Self((u64::from(tag) << OFFSET_BITS) | offset))
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
        for (tag, offset) in [(0u8, 0u64), (1, 4096), (255, MAX_OFFSET), (7, 1 << 40)] {
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
}
