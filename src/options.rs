use crate::{Error, Result, store::DEFAULT_SEGMENT_SIZE};

/// Default upper bound on the index's memory map. This reserves address
/// space, not disk: the index file grows on demand within the limit.
pub const DEFAULT_INDEX_MAP_SIZE: u64 = 4 * 1024 * 1024 * 1024 * 1024;

/// The configuration a database is created with.
///
/// Passed to [`DbRW::open_or_create`] and [`DbRO::open_or_create`], which
/// create a database with exactly this configuration when there is none at the
/// path, and otherwise require the one already there to match it. Nothing here
/// is needed to open a database that exists: [`DbRW::open`] and [`DbRO::open`]
/// read the whole configuration back from disk.
///
/// ```no_run
/// # fn main() -> Result<(), bytesklad::Error> {
/// use bytesklad::{DbRW, Options};
///
/// // Values of exactly 32 or 64 bytes get their own bucket; everything else
/// // goes to the variable-length bucket.
/// let db = DbRW::<32>::open_or_create("./db", &Options::new().buckets([32, 64]))?;
/// # Ok(()) }
/// ```
///
/// [`DbRW::open`]: crate::DbRW::open
/// [`DbRW::open_or_create`]: crate::DbRW::open_or_create
/// [`DbRO::open`]: crate::DbRO::open
/// [`DbRO::open_or_create`]: crate::DbRO::open_or_create
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub(crate) buckets: Vec<usize>,
    pub(crate) segment_size: u64,
    pub(crate) index_map_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            buckets: Vec::new(),
            segment_size: DEFAULT_SEGMENT_SIZE,
            index_map_size: DEFAULT_INDEX_MAP_SIZE,
        }
    }
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a bucket for values of exactly `record_size` bytes.
    ///
    /// Values of that length are then stored unframed, and their length is
    /// recovered from the bucket instead of from the index. Worth doing for
    /// lengths that make up a large share of the data set; pointless for rare
    /// ones, which cost a directory and a segment file each.
    pub fn bucket(mut self, record_size: usize) -> Self {
        if !self.buckets.contains(&record_size) {
            self.buckets.push(record_size);
        }
        self
    }

    /// Declare several buckets at once. See [`bucket`](Self::bucket).
    pub fn buckets(mut self, record_sizes: impl IntoIterator<Item = usize>) -> Self {
        for record_size in record_sizes {
            self = self.bucket(record_size);
        }
        self
    }

    /// Cap on the size of one segment file, defaulting to
    /// [`DEFAULT_SEGMENT_SIZE`].
    ///
    /// Larger segments mean fewer, bigger files; smaller ones mean
    /// finer-grained backup and replication units, and more open file
    /// descriptors.
    pub fn segment_size(mut self, bytes: u64) -> Self {
        self.segment_size = bytes;
        self
    }

    /// Upper bound on the index's memory map, defaulting to
    /// [`DEFAULT_INDEX_MAP_SIZE`]. Raise it if the index may exceed 4 TiB.
    ///
    /// Unlike the rest of these settings this one is not part of the
    /// database's stored configuration, so it is never matched against disk.
    pub fn index_map_size(mut self, bytes: u64) -> Self {
        self.index_map_size = bytes;
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.segment_size == 0 {
            return Err(Error::ZeroSegmentSize);
        }
        if self.buckets.contains(&0) {
            return Err(Error::ZeroBucket);
        }
        Ok(())
    }
}
