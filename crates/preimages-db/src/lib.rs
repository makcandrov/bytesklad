//! Preimage database: `keccak256(data) → data`.
//!
//! Uses [`flat_store`] for bulk byte storage and MDBX for the hash index.
//! Each MDBX entry is 12 bytes: `[8-byte offset][4-byte len]`.

use std::fs;
use std::path::PathBuf;

use flat_store::FlatStore;
pub use flat_store::{Mode, RO, RW};
use libmdbx::{
    Database, DatabaseOptions, Mode as MdbxMode, NoWriteMap, ReadWriteOptions, SyncMode,
    TableFlags, WriteFlags,
};

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

// ── Error ────────────────────────────────────────────────────────────

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

// ── Value encoding ──────────────────────────────────────────────────

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

// ── Config ───────────────────────────────────────────────────────────

pub struct PreimageDbConfig {
    /// Root directory for all data (MDBX files live here, flat files in a
    /// `data/` subdirectory).
    pub path: PathBuf,
    /// Bucket sizes for the flat store. Preimages whose length matches a
    /// bucket go into a dedicated file; all others go into the unsized file.
    pub buckets: Vec<usize>,
}

impl PreimageDbConfig {
    pub fn new(path: impl Into<PathBuf>, buckets: Vec<usize>) -> Self {
        Self {
            path: path.into(),
            buckets,
        }
    }
}

// ── PreimageDb ───────────────────────────────────────────────────────

pub struct PreimageDb<M: Mode = RW> {
    mdbx: Database<NoWriteMap>,
    store: FlatStore<M>,
}

impl PreimageDb<RW> {
    /// Open the database in read-write mode. Only one writer may hold the
    /// database open at a time.
    pub fn open(config: PreimageDbConfig) -> Result<Self> {
        fs::create_dir_all(&config.path)?;

        let mdbx = Database::<NoWriteMap>::open_with_options(
            &config.path,
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

        let store = FlatStore::open(config.path.join("data"), config.buckets)?;

        Ok(Self { mdbx, store })
    }

    /// Insert a single preimage. Returns `true` if newly inserted.
    pub fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool> {
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

    /// Insert a batch of preimages. Returns the number of new entries.
    pub fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; 32], &'a [u8])>,
    ) -> Result<usize> {
        let mut to_insert: Vec<_> = Vec::new();
        {
            let tx = self.mdbx.begin_ro_txn()?;
            let table = tx.open_table(Some(MDBX_TABLE))?;
            for (hash, data) in entries {
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

impl PreimageDb<RO> {
    /// Open the database in read-only mode. Any number of read-only handles
    /// may coexist with a single writer.
    pub fn open_read_only(config: PreimageDbConfig) -> Result<Self> {
        let mdbx = Database::<NoWriteMap>::open_with_options(
            &config.path,
            DatabaseOptions {
                mode: MdbxMode::ReadOnly,
                max_tables: Some(1),
                ..Default::default()
            },
        )?;

        let store = FlatStore::<RO>::open_read_only(config.path.join("data"), config.buckets)?;

        Ok(Self { mdbx, store })
    }
}

impl<M: Mode> PreimageDb<M> {
    /// Look up a preimage by its keccak256 hash.
    pub fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        let raw: Option<Vec<u8>> = tx.get(&table, hash.as_slice())?;

        let Some(raw) = raw else {
            return Ok(None);
        };

        let (offset, len) = decode_value(&raw)?;
        let data = self.store.read_to_vec(offset, len)?;
        Ok(Some(data))
    }

    /// Check if a hash exists in the index.
    pub fn contains(&self, hash: &[u8; 32]) -> Result<bool> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        Ok(tx.get::<Vec<u8>>(&table, hash.as_slice())?.is_some())
    }

    /// Total number of preimages stored.
    pub fn len(&self) -> Result<usize> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        Ok(tx.table_stat(&table)?.entries())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Returns the entry with the largest hash less than or equal to `hash`,
    /// or `None` if no such entry exists.
    pub fn nearest_lower(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        let mut cursor = tx.cursor(&table)?;

        let result = match cursor.set_range::<Vec<u8>, Vec<u8>>(hash.as_slice())? {
            Some((key, value)) if key.as_slice() == hash.as_slice() => Some((key, value)),
            _ => cursor.prev::<Vec<u8>, Vec<u8>>()?,
        };

        self.read_cursor_result(result)
    }

    /// Returns the entry with the smallest hash greater than or equal to `hash`,
    /// or `None` if no such entry exists.
    pub fn nearest_upper(&self, hash: &[u8; 32]) -> Result<Option<([u8; 32], Vec<u8>)>> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        let mut cursor = tx.cursor(&table)?;

        let result = cursor.set_range::<Vec<u8>, Vec<u8>>(hash.as_slice())?;

        self.read_cursor_result(result)
    }

    fn read_cursor_result(
        &self,
        result: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<Option<([u8; 32], Vec<u8>)>> {
        let Some((key, value)) = result else {
            return Ok(None);
        };
        let hash: [u8; 32] = key.try_into().map_err(|_| Error::CorruptIndex)?;
        let (offset, len) = decode_value(&value)?;
        let data = self.store.read_to_vec(offset, len)?;
        Ok(Some((hash, data)))
    }
}
