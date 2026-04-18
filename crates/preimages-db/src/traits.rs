use crate::Result;

/// Read-only access to a preimage database.
pub trait PreimagesDbRead {
    /// Look up a preimage by its keccak256 hash.
    fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>>;

    /// Check if a hash exists in the index.
    fn contains(&self, hash: &[u8; 32]) -> Result<bool>;

    /// Total number of preimages stored.
    fn len(&self) -> Result<usize>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Returns the entry with the largest hash less than or equal to `hash`,
    /// or `None` if no such entry exists.
    fn nearest_lower(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>>;

    /// Returns the entry with the smallest hash greater than or equal to `hash`,
    /// or `None` if no such entry exists.
    fn nearest_upper(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>>;
}

/// Read-write access to a preimage database.
///
/// # Failure semantics
///
/// Writes are ordered data-then-index: the MDBX index never references data
/// absent from the flat store. The reverse (flat-store bytes with no index
/// entry) can occur on write failure — those bytes are a silent space leak
/// with no garbage-collection path. This trade is intentional: losing a
/// pointer into missing data would surface as a read-time error, whereas
/// leaked bytes are merely wasted disk.
pub trait PreimagesDbWrite: PreimagesDbRead {
    /// Insert a single preimage. Returns `true` if newly inserted.
    ///
    /// Each call fsyncs both the data file and the MDBX index so the entry is
    /// durable on return. For many inserts, prefer [`insert_batch`] — it
    /// performs a single fsync for the whole batch.
    ///
    /// On failure after the data fsync but before the index commit, the
    /// written bytes are leaked in the flat store (see trait-level docs).
    ///
    /// [`insert_batch`]: PreimagesDbWrite::insert_batch
    fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool>;

    /// Insert a batch of preimages. Returns the number of new entries.
    ///
    /// The whole batch is made durable with a single fsync per store, so this
    /// is the preferred path for bulk ingest.
    ///
    /// On any error the MDBX transaction is aborted and no index entries are
    /// committed. Bytes appended to the flat store earlier in the batch sit
    /// past the checkpoint until the final sync, so in most in-batch
    /// failures they are truncated on the next open — no leak. A failure
    /// between the final store sync and the MDBX commit still leaks those
    /// bytes (see trait-level docs).
    fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; 32], &'a [u8])>,
    ) -> Result<usize>;
}
