use std::io;

/// Read-only access to a flat store.
pub trait FlatStoreRead {
    /// Read exactly `buf.len()` bytes from `offset` into `buf`.
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<(), io::Error>;

    /// Read `len` bytes starting at `offset` into a newly allocated `Vec`.
    fn read_to_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, io::Error> {
        let mut buf = vec![0; len];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }

    /// Read exactly `N` bytes starting at `offset` into a stack-allocated array.
    fn read_to_array<const N: usize>(&self, offset: u64) -> Result<[u8; N], io::Error> {
        let mut buf = [0; N];
        self.read(&mut buf, offset)?;
        Ok(buf)
    }
}

/// Read-write access to a flat store.
pub trait FlatStoreWrite: FlatStoreRead {
    /// Append `data` to the appropriate bucket and return the offset at which
    /// it was written.
    fn insert(&self, data: &[u8]) -> Result<u64, io::Error>;

    /// Flush all buffered writes to disk and persist a checkpoint so the store
    /// can recover its write offsets on reopen.
    fn sync(&self) -> Result<(), io::Error>;
}
