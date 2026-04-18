#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../../../README.md")]

//! Preimage database: `keccak256(data) → data`.
//!
//! Uses [`flat_store`] for bulk byte storage and MDBX for the hash index.
//! Each MDBX entry is 12 bytes: `[8-byte offset][4-byte len]`.

use std::{fs, path::Path};

use flat_store::{FlatStoreRO, FlatStoreRW, FlatStoreRead, FlatStoreWrite};
use libmdbx::{
    Database, DatabaseOptions, Mode as MdbxMode, NoWriteMap, ReadWriteOptions, SyncMode,
    TableFlags, WriteFlags,
};

mod traits;
pub use traits::{PreimagesDbRead, PreimagesDbWrite};

#[cfg(feature = "sdecode")]
mod sdecode;

/// Name of the MDBX table that stores the `hash → (offset, len)` index.
const MDBX_TABLE: &str = "preimages";

/// Size in bytes of an MDBX value: 8-byte little-endian offset followed by
/// 4-byte little-endian length.
const VALUE_LEN: usize = 12;

/// Encoded MDBX value: `[8-byte offset][4-byte len]`, little-endian.
type EncodedValue = [u8; VALUE_LEN];

/// Maximum size (1 TB) of the MDBX memory map. This is a virtual address
/// reservation, not on-disk allocation — the file grows on demand within
/// this limit. Sized to comfortably exceed any expected index footprint.
const MDBX_MAP_SIZE: isize = 1024 * 1024 * 1024 * 1024;

// The map size above doesn't fit in `isize` on 32-bit targets, and MDBX itself
// is impractical with a 2 GB address space anyway. Fail loudly at compile time
// rather than silently overflowing or crashing at runtime.
const _: () = assert!(usize::BITS >= 64, "preimages-db requires a 64-bit target");

// Guarantee the RW handle can be shared across threads via `Arc`: multi-threaded
// writers are a supported use case.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<PreimagesDbRW>();
    assert_sync::<PreimagesDbRW>();
    assert_send::<PreimagesDbRO>();
    assert_sync::<PreimagesDbRO>();
};

/// Read-only preimage database. Any number of read-only handles may coexist
/// with a single writer.
pub struct PreimagesDbRO {
    mdbx: Database<NoWriteMap>,
    store: FlatStoreRO,
}

impl std::fmt::Debug for PreimagesDbRO {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreimagesDbRO")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Read-write preimage database. Only one writer may hold the database open
/// at a time.
pub struct PreimagesDbRW {
    mdbx: Database<NoWriteMap>,
    store: FlatStoreRW,
}

impl std::fmt::Debug for PreimagesDbRW {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreimagesDbRW")
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

    #[error("preimage too large: {0} bytes exceeds u32::MAX")]
    PreimageTooLarge(usize),
}

pub type Result<T> = std::result::Result<T, Error>;

fn encode_value(offset: u64, len: u32) -> EncodedValue {
    let mut buf = [0u8; VALUE_LEN];
    buf[..8].copy_from_slice(&offset.to_le_bytes());
    buf[8..].copy_from_slice(&len.to_le_bytes());
    buf
}

fn decode_value(raw: &EncodedValue) -> (u64, usize) {
    let offset = u64::from_le_bytes(raw[..8].try_into().unwrap());
    let len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    (offset, len)
}

impl PreimagesDbRW {
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

impl PreimagesDbRO {
    /// Open the database in read-only mode. Bucket layout is discovered from
    /// disk, so the caller doesn't need to know what buckets the writer used.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mdbx = Database::<NoWriteMap>::open_with_options(
            &path,
            DatabaseOptions {
                mode: MdbxMode::ReadOnly,
                max_tables: Some(1),
                ..Default::default()
            },
        )?;

        let store = FlatStoreRO::open(path.as_ref().join("data"))?;

        Ok(Self { mdbx, store })
    }
}

impl PreimagesDbRead for PreimagesDbRW {
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

impl PreimagesDbWrite for PreimagesDbRW {
    fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool> {
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;

        if tx.get::<()>(&table, hash.as_slice())?.is_some() {
            return Ok(false);
        }

        let len = u32::try_from(data.len()).map_err(|_| Error::PreimageTooLarge(data.len()))?;

        let offset = self.store.insert(data)?;
        self.store.sync()?;

        let value = encode_value(offset, len);
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
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;

        let mut count = 0;
        for (hash, data) in entries {
            if tx.get::<()>(&table, hash.as_slice())?.is_some() {
                continue;
            }
            let len = u32::try_from(data.len()).map_err(|_| Error::PreimageTooLarge(data.len()))?;
            let offset = self.store.insert(data)?;
            let value = encode_value(offset, len);
            tx.put(
                &table,
                hash.as_slice(),
                value.as_slice(),
                WriteFlags::NO_OVERWRITE,
            )?;
            count += 1;
        }

        if count == 0 {
            return Ok(0);
        }

        self.store.sync()?;
        tx.commit()?;
        self.mdbx.sync(true)?;

        Ok(count)
    }
}

impl PreimagesDbRead for PreimagesDbRO {
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
    let raw: Option<EncodedValue> = tx.get(&table, hash.as_slice())?;

    let Some(raw) = raw else {
        return Ok(None);
    };

    let (offset, len) = decode_value(&raw);
    let data = store.read_to_vec(offset, len)?;
    Ok(Some(data))
}

fn db_contains(mdbx: &Database<NoWriteMap>, hash: &[u8; 32]) -> Result<bool> {
    let tx = mdbx.begin_ro_txn()?;
    let table = tx.open_table(Some(MDBX_TABLE))?;
    Ok(tx.get::<()>(&table, hash.as_slice())?.is_some())
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

    let result = match cursor.set_range::<[u8; 32], EncodedValue>(hash.as_slice())? {
        Some((key, value)) if &key == hash => Some((key, value)),
        // When `set_range` returns `None` (hash exceeds every stored key),
        // MDBX leaves the cursor past the end so `prev` returns the last
        // entry. Pinned by `nearest_lower_hash_above_all_keys`.
        _ => cursor.prev::<[u8; 32], EncodedValue>()?,
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

    let result = cursor.set_range::<[u8; 32], EncodedValue>(hash.as_slice())?;

    read_cursor_result(store, result)
}

fn read_cursor_result(
    store: &impl FlatStoreRead,
    result: Option<([u8; 32], EncodedValue)>,
) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let Some((hash, value)) = result else {
        return Ok(None);
    };
    let (offset, len) = decode_value(&value);
    let data = store.read_to_vec(offset, len)?;
    Ok(Some((hash, data)))
}
