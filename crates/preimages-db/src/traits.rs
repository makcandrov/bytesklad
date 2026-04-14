use crate::Result;

/// Read-only access to a preimage database.
pub trait PreimageDbRead {
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
pub trait PreimageDbWrite: PreimageDbRead {
    /// Insert a single preimage. Returns `true` if newly inserted.
    fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool>;

    /// Insert a batch of preimages. Returns the number of new entries.
    fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; 32], &'a [u8])>,
    ) -> Result<usize>;
}
