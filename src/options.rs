use std::path::Path;

use crate::{DbRO, DbRW, Result};

/// Default upper bound on the index's memory map. This reserves address
/// space, not disk: the index file grows on demand within the limit.
pub const DEFAULT_INDEX_MAP_SIZE: u64 = 4 * 1024 * 1024 * 1024 * 1024;

/// How a database is opened.
///
/// Every setting has a usable default, so the shortest way to open a database
/// with 32-byte keys and no size buckets is [`DbRW::open`].
///
/// ```no_run
/// # fn main() -> Result<(), bytesklad::Error> {
/// use bytesklad::Options;
///
/// // Values of exactly 32 or 64 bytes get their own bucket; everything else
/// // goes to the variable-length bucket.
/// let db = Options::new().buckets([32, 64]).open::<32>("./db")?;
/// # Ok(()) }
/// ```
///
/// Buckets are *declarative*: opening an existing database asks for those
/// buckets to exist. Ones already present are reused, new ones are added, and
/// none are ever removed — so passing no buckets to an existing database keeps
/// whatever it already has rather than discarding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub(crate) buckets: Vec<usize>,
    pub(crate) segment_size: Option<u64>,
    pub(crate) index_map_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            buckets: Vec::new(),
            segment_size: None,
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

    /// Cap on the size of one segment file, defaulting to 4 GiB.
    ///
    /// Fixed when the database is created and rejected on later opens if it
    /// disagrees, because stored pointers are decoded against it. Larger
    /// segments mean fewer, bigger files; smaller ones mean finer-grained
    /// backup and replication units, and more open file descriptors.
    pub fn segment_size(mut self, bytes: u64) -> Self {
        self.segment_size = Some(bytes);
        self
    }

    /// Upper bound on the index's memory map, defaulting to
    /// [`DEFAULT_INDEX_MAP_SIZE`]. Raise it if the index may exceed 4 TiB.
    pub fn index_map_size(mut self, bytes: u64) -> Self {
        self.index_map_size = bytes;
        self
    }

    /// Open for reading and writing, creating the database if absent.
    ///
    /// `K` is the key length in bytes, fixed at creation.
    pub fn open<const K: usize>(&self, path: impl AsRef<Path>) -> Result<DbRW<K>> {
        DbRW::open_with(path, self)
    }

    /// Open an existing database for reading.
    ///
    /// Bucket layout, segment size and key length are all read back from the
    /// database itself, so none of them need to be declared here; only
    /// [`index_map_size`](Self::index_map_size) still applies.
    pub fn open_read_only<const K: usize>(&self, path: impl AsRef<Path>) -> Result<DbRO<K>> {
        DbRO::open_with(path, self)
    }
}
