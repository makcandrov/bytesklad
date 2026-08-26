use crate::Result;

/// Read access to a database with `K`-byte keys.
///
/// Keys are ordered lexicographically, which for fixed-size keys is the same
/// as big-endian numeric order.
pub trait DbRead<const K: usize> {
    /// Look up the value stored under `key`.
    fn get(&self, key: &[u8; K]) -> Result<Option<Vec<u8>>>;

    /// Whether `key` is present, without reading its value.
    fn contains(&self, key: &[u8; K]) -> Result<bool>;

    /// Number of entries in the database.
    fn len(&self) -> Result<usize>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The entry with the smallest key, or `None` if the database is empty.
    fn first(&self) -> Result<Option<([u8; K], Vec<u8>)>>;

    /// The entry with the largest key, or `None` if the database is empty.
    fn last(&self) -> Result<Option<([u8; K], Vec<u8>)>>;

    /// The entry with the largest key less than or equal to `key`.
    fn nearest_lower(&self, key: &[u8; K]) -> Result<Option<([u8; K], Vec<u8>)>>;

    /// The entry with the smallest key greater than or equal to `key`.
    fn nearest_upper(&self, key: &[u8; K]) -> Result<Option<([u8; K], Vec<u8>)>>;
}

/// Write access to a database with `K`-byte keys.
///
/// The database is insert-only: an existing key is never overwritten and no
/// entry is ever removed.
///
/// # Failure semantics
///
/// Writes are ordered store-then-index: values are made durable before the
/// index entry naming them commits, so the index never points at bytes that
/// are missing. The reverse — bytes in the store with no index entry — can
/// happen, and those bytes are simply never read again. Most such bytes sit
/// past the durable frontier and are truncated on the next writer open; only
/// a failure in the narrow window between the store sync and the index commit
/// leaks them permanently. The trade is deliberate: a dangling pointer would
/// surface as a read error, whereas leaked bytes only cost disk.
pub trait DbWrite<const K: usize>: DbRead<K> {
    /// Insert one entry, returning `false` if `key` was already present.
    ///
    /// Durable on return. For more than a handful of entries prefer
    /// [`insert_batch`](Self::insert_batch), which flushes once for the whole
    /// batch instead of once per entry.
    fn insert(&self, key: &[u8; K], value: &[u8]) -> Result<bool>;

    /// Insert many entries, returning how many were new.
    ///
    /// Keys already present, in the database or earlier in the batch, are
    /// skipped. The whole batch becomes durable with a single flush of the
    /// store and a single flush of the index.
    ///
    /// On error the index transaction is aborted and no entry is committed.
    fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; K], &'a [u8])>,
    ) -> Result<usize>;
}
