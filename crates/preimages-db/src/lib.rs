//! Preimage database: `keccak256(data) → data`.
//!
//! Uses [`flat_store`] for bulk byte storage and MDBX for the hash index.
//! Each MDBX entry is 12 bytes: `[8-byte Ref][4-byte len]`.

use std::fs;
use std::path::PathBuf;

use flat_store::{FlatStore, FlatStoreConfig, Ref};
use libmdbx::{
    Database, DatabaseOptions, Mode, NoWriteMap, ReadWriteOptions, SyncMode, TableFlags, WriteFlags,
};
use thiserror::Error;

pub use flat_store::FileGroup;

const MDBX_TABLE: &str = "preimages";
const VALUE_LEN: usize = 12;

// ── Error ────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
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

fn encode_value(r: Ref, len: u32) -> [u8; VALUE_LEN] {
    let mut buf = [0u8; VALUE_LEN];
    buf[..8].copy_from_slice(&r.to_le_bytes());
    buf[8..].copy_from_slice(&len.to_le_bytes());
    buf
}

fn decode_value(raw: &[u8]) -> std::result::Result<(Ref, usize), Error> {
    if raw.len() != VALUE_LEN {
        return Err(Error::CorruptIndex);
    }
    let r = Ref::from_le_bytes(raw[..8].try_into().unwrap());
    let len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    Ok((r, len))
}

// ── Config ───────────────────────────────────────────────────────────

pub struct PreimageDbConfig {
    /// Root directory for all data (MDBX files live here, flat files in a
    /// `data/` subdirectory).
    pub path: PathBuf,
    /// How to split preimage data across flat files.
    pub groups: Vec<FileGroup>,
    /// MDBX map size. Default: 64 GB.
    pub mdbx_map_size: usize,
}

impl PreimageDbConfig {
    pub fn new(path: impl Into<PathBuf>, groups: Vec<FileGroup>) -> Self {
        Self {
            path: path.into(),
            groups,
            mdbx_map_size: 64 * 1024 * 1024 * 1024,
        }
    }
}

// ── PreimageDb ───────────────────────────────────────────────────────

pub struct PreimageDb {
    mdbx: Database<NoWriteMap>,
    store: FlatStore,
}

impl PreimageDb {
    pub fn open(config: PreimageDbConfig) -> Result<Self> {
        fs::create_dir_all(&config.path)?;

        let mdbx = Database::<NoWriteMap>::open_with_options(
            &config.path,
            DatabaseOptions {
                mode: Mode::ReadWrite(ReadWriteOptions {
                    sync_mode: SyncMode::SafeNoSync,
                    max_size: Some(config.mdbx_map_size as isize),
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

        let store = FlatStore::open(FlatStoreConfig {
            path: config.path.join("data"),
            groups: config.groups,
        })?;

        Ok(Self { mdbx, store })
    }

    /// Insert a single preimage. Returns `true` if newly inserted.
    pub fn insert(&self, hash: &[u8; 32], data: &[u8]) -> Result<bool> {
        if self.contains(hash)? {
            return Ok(false);
        }

        let r = self.store.insert(data)?;
        self.store.sync()?;

        let value = encode_value(r, data.len() as u32);
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        tx.put(
            &table,
            hash.as_slice(),
            value.as_slice(),
            WriteFlags::NO_OVERWRITE,
        )?;
        tx.commit()?;

        Ok(true)
    }

    /// Insert a batch of preimages. Returns the number of new entries.
    pub fn insert_batch(&self, entries: &[([u8; 32], Vec<u8>)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let mut to_insert = Vec::with_capacity(entries.len());
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

        let data_slices: Vec<&[u8]> = to_insert.iter().map(|(_, d)| d.as_slice()).collect();
        let refs = self.store.insert_many(&data_slices)?;
        self.store.sync()?;

        let count = to_insert.len();
        let tx = self.mdbx.begin_rw_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        for (i, (hash, data)) in to_insert.iter().enumerate() {
            let value = encode_value(refs[i], data.len() as u32);
            tx.put(
                &table,
                hash.as_slice(),
                value.as_slice(),
                WriteFlags::NO_OVERWRITE,
            )?;
        }
        tx.commit()?;

        Ok(count)
    }

    /// Look up a preimage by its keccak256 hash.
    pub fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let tx = self.mdbx.begin_ro_txn()?;
        let table = tx.open_table(Some(MDBX_TABLE))?;
        let raw: Option<Vec<u8>> = tx.get(&table, hash.as_slice())?;

        let Some(raw) = raw else {
            return Ok(None);
        };

        let (r, len) = decode_value(&raw)?;
        let data = self.store.get(r, len)?;
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

    /// Flush all data files and MDBX to disk.
    pub fn sync(&self) -> Result<()> {
        self.store.sync()?;
        self.mdbx.sync(true)?;
        Ok(())
    }
}
