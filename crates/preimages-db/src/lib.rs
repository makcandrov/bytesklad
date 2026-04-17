//! Preimage database: `keccak256(data) → data`.
//!
//! Uses [`flat_store`] for bulk byte storage and MDBX for the hash index.
//! Each MDBX entry is 12 bytes: `[8-byte offset][4-byte len]`.

use std::{collections::HashSet, fs, path::Path};

use flat_store::{FlatStoreRO, FlatStoreRW, FlatStoreRead, FlatStoreWrite};
use libmdbx::{
    Database, DatabaseOptions, Mode as MdbxMode, NoWriteMap, ReadWriteOptions, SyncMode,
    TableFlags, WriteFlags,
};

mod traits;
pub use traits::{PreimageDbRead, PreimageDbWrite};

#[cfg(feature = "sdecode")]
mod sdecode;

/// Name of the MDBX table that stores the `hash → (offset, len)` index.
const MDBX_TABLE: &str = "preimages";

/// Size in bytes of an MDBX value: 8-byte little-endian offset followed by
/// 4-byte little-endian length.
const VALUE_LEN: usize = 12;

/// Maximum size (1 TB) of the MDBX memory map. This is a virtual address
/// reservation, not on-disk allocation — the file grows on demand within
/// this limit. Sized to comfortably exceed any expected index footprint.
const MDBX_MAP_SIZE: isize = 1024 * 1024 * 1024 * 1024;

// The map size above doesn't fit in `isize` on 32-bit targets, and MDBX itself
// is impractical with a 2 GB address space anyway. Fail loudly at compile time
// rather than silently overflowing or crashing at runtime.
const _: () = assert!(
    usize::BITS >= 64,
    "preimages-db requires a 64-bit target"
);

/// Read-only preimage database. Any number of read-only handles may coexist
/// with a single writer.
pub struct PreimageDbRO {
    mdbx: Database<NoWriteMap>,
    store: FlatStoreRO,
}

impl std::fmt::Debug for PreimageDbRO {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreimageDbRO")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Read-write preimage database. Only one writer may hold the database open
/// at a time.
pub struct PreimageDbRW {
    mdbx: Database<NoWriteMap>,
    store: FlatStoreRW,
}

impl std::fmt::Debug for PreimageDbRW {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreimageDbRW")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("MDBX error: {0}")]
    Mdbx(#[from] libmdbx::Error),

    #[error("store error: {0}")]
    Store(#[from] flat_store::Error),

    #[error("corrupt index entry")]
    CorruptIndex,
}

pub type Result<T> = std::result::Result<T, Error>;

fn encode_value(offset: u64, len: u32) -> [u8; VALUE_LEN] {
    let mut buf = [0u8; VALUE_LEN];
    buf[..8].copy_from_slice(&offset.to_le_bytes());
    buf[8..].copy_from_slice(&len.to_le_bytes());
    buf
}

fn decode_value(raw: &[u8]) -> Result<(u64, usize)> {
    if raw.len() != VALUE_LEN {
        return Err(Error::CorruptIndex);
    }
    let offset = u64::from_le_bytes(raw[..8].try_into().unwrap());
    let len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    Ok((offset, len))
}

impl PreimageDbRW {
    /// Open the database in read-write mode.
    pub fn open(path: impl AsRef<Path>, buckets: impl IntoIterator<Item = usize>) -> Result<Self> {
        fs::create_dir_all(&path)?;

        let mdbx = Database::<NoWriteMap>::open_with_options(
            &path,
            DatabaseOptions {
                mode: MdbxMode::ReadWrite(ReadWriteOptions {
                    sync_mode: SyncMode::SafeNoSync,
                    max_size: Some(MDBX_MAP_SIZE),
                    ..Default::default()
                }),
                max_tables: Some(1),
                ..Default::default()
            },
        )?;

        {
            let tx = mdbx.begin_rw_txn()?;
            tx.create_table(Some(MDBX_TABLE), TableFlags::empty())?;
            tx.commit()?;
        }

        let store = FlatStoreRW::open(path.as_ref().join("data"), buckets)?;

        Ok(Self { mdbx, store })
    }
}

impl PreimageDbRO {
    /// Open the database in read-only mode.
    pub fn open(path: impl AsRef<Path>, buckets: impl IntoIterator<Item = usize>) -> Result<Self> {
        let mdbx = Database::<NoWriteMap>::open_with_options(
            &path,
            DatabaseOptions {
                mode: MdbxMode::ReadOnly,
                max_tables: Some(1),
                ..Default::default()
            },
        )?;

        let store = FlatStoreRO::open(path.as_ref().join("data"), buckets)?;

        Ok(Self { mdbx, store })
    }
}

impl PreimageDbRead for PreimageDbRW {
    fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        db_get(&self.mdbx, &self.store, hash)
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        db_contains(&self.mdbx, hash)
    }

    fn len(&self) -> Result<usize> {
        db_len(&self.mdbx)
    }

    fn nearest_lower(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        db_nearest_lower(&self.mdbx, &self.store, hash)
    }

    fn nearest_upper(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        db_nearest_upper(&self.mdbx, &self.store, hash)
    }
}

impl PreimageDbWrite for PreimageDbRW {
    fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool> {
        if self.contains(hash)? {
            return Ok(false);
        }

        let offset = self.store.insert(data)?;
        self.store.sync()?;

        let value = encode_value(offset, data.len() as u32);
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        tx.put(
            &table,
            hash.as_slice(),
            value.as_slice(),
            WriteFlags::NO_OVERWRITE,
        )?;
        tx.commit()?;
        self.mdbx.sync(true)?;

        Ok(true)
    }

    fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; 32], &'a [u8])>,
    ) -> Result<usize> {
        let mut seen = HashSet::<[u8; 32]>::default();
        let mut to_insert: Vec<(&'a [u8; 32], &'a [u8])> = Vec::new();
        {
            let tx = self.mdbx.begin_ro_txn()?;
            let table = tx.open_table(Some(MDBX_TABLE))?;
            for (hash, data) in entries {
                if !seen.insert(*hash) {
                    continue;
                }
                if tx.get::<Vec<u8>>(&table, hash.as_slice())?.is_none() {
                    to_insert.push((hash, data));
                }
            }
        }

        if to_insert.is_empty() {
            return Ok(0);
        }

        let mut offsets = Vec::with_capacity(to_insert.len());
        for (_, data) in &to_insert {
            offsets.push(self.store.insert(data)?);
        }
        self.store.sync()?;

        let count = to_insert.len();
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        for (i, (hash, data)) in to_insert.iter().enumerate() {
            let value = encode_value(offsets[i], data.len() as u32);
            tx.put(
                &table,
                hash.as_slice(),
                value.as_slice(),
                WriteFlags::NO_OVERWRITE,
            )?;
        }
        tx.commit()?;
        self.mdbx.sync(true)?;

        Ok(count)
    }
}

impl PreimageDbRead for PreimageDbRO {
    fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        db_get(&self.mdbx, &self.store, hash)
    }

    fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        db_contains(&self.mdbx, hash)
    }

    fn len(&self) -> Result<usize> {
        db_len(&self.mdbx)
    }

    fn nearest_lower(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        db_nearest_lower(&self.mdbx, &self.store, hash)
    }

    fn nearest_upper(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        db_nearest_upper(&self.mdbx, &self.store, hash)
    }
}

fn db_get(
    mdbx: &Database<NoWriteMap>,
    store: &impl FlatStoreRead,
    hash: &[u8; 32],
) -> Result<Option<Vec<u8>>> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    let raw: Option<Vec<u8>> = tx.get(&table, hash.as_slice())?;

    let Some(raw) = raw else {
        return Ok(None);
    };

    let (offset, len) = decode_value(&raw)?;
    let data = store.read_to_vec(offset, len)?;
    Ok(Some(data))
}

fn db_contains(mdbx: &Database<NoWriteMap>, hash: &[u8; 32]) -> Result<bool> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    Ok(tx.get::<Vec<u8>>(&table, hash.as_slice())?.is_some())
}

fn db_len(mdbx: &Database<NoWriteMap>) -> Result<usize> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    Ok(tx.table_stat(&table)?.entries())
}

fn db_nearest_lower(
    mdbx: &Database<NoWriteMap>,
    store: &impl FlatStoreRead,
    hash: &[u8; 32],
) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    let mut cursor = tx.cursor(&table)?;

    let result = match cursor.set_range::<Vec<u8>, Vec<u8>>(hash.as_slice())? {
        Some((key, value)) if key.as_slice() == hash.as_slice() => Some((key, value)),
        _ => cursor.prev::<Vec<u8>, Vec<u8>>()?,
    };

    read_cursor_result(store, result)
}

fn db_nearest_upper(
    mdbx: &Database<NoWriteMap>,
    store: &impl FlatStoreRead,
    hash: &[u8; 32],
) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    let mut cursor = tx.cursor(&table)?;

    let result = cursor.set_range::<Vec<u8>, Vec<u8>>(hash.as_slice())?;

    read_cursor_result(store, result)
}

fn read_cursor_result(
    store: &impl FlatStoreRead,
    result: Option<(Vec<u8>, Vec<u8>)>,
) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let Some((key, value)) = result else {
        return Ok(None);
    };
    let hash: [u8; 32] = key.try_into().map_err(|_| Error::CorruptIndex)?;
    let (offset, len) = decode_value(&value)?;
    let data = store.read_to_vec(offset, len)?;
    Ok(Some((hash, data)))
}
