use std::{fs, path::Path};

use libmdbx::{
    Database, DatabaseOptions, Mode, NoWriteMap, ReadWriteOptions, SyncMode, TableFlags,
};

use crate::{Result, store::Pointer};

pub(crate) const TABLE: &str = "bytesklad";

/// Encoded index value: an eight-byte little-endian [`Pointer`].
type Encoded = [u8; 8];

/// The ordered key index: `key -> Pointer`, backed by MDBX.
///
/// MDBX is opened in `SafeNoSync` mode and flushed explicitly after each
/// committed batch, so a batch costs one flush rather than one per entry.
pub(crate) struct Index {
    db: Database<NoWriteMap>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index").finish_non_exhaustive()
    }
}

impl Index {
    pub fn open_writable(dir: &Path, map_size: u64) -> Result<Self> {
        fs::create_dir_all(dir)?;

        let db = Database::<NoWriteMap>::open_with_options(
            dir,
            DatabaseOptions {
                mode: Mode::ReadWrite(ReadWriteOptions {
                    sync_mode: SyncMode::SafeNoSync,
                    max_size: Some(map_size as isize),
                    ..Default::default()
                }),
                max_tables: Some(1),
                ..Default::default()
            },
        )?;

        let tx = db.begin_rw_txn()?;
        tx.create_table(Some(TABLE), TableFlags::empty())?;
        tx.commit()?;

        Ok(Self { db })
    }

    pub fn open_read_only(dir: &Path) -> Result<Self> {
        let db = Database::<NoWriteMap>::open_with_options(
            dir,
            DatabaseOptions {
                mode: Mode::ReadOnly,
                max_tables: Some(1),
                ..Default::default()
            },
        )?;
        Ok(Self { db })
    }

    pub fn db(&self) -> &Database<NoWriteMap> {
        &self.db
    }

    pub fn sync(&self) -> Result<()> {
        self.db.sync(true)?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Pointer>> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        Ok(tx.get::<Encoded>(&table, key)?.map(Pointer::from_le_bytes))
    }

    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        Ok(tx.get::<()>(&table, key)?.is_some())
    }

    pub fn len(&self) -> Result<usize> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        Ok(tx.table_stat(&table)?.entries())
    }

    pub fn first<const K: usize>(&self) -> Result<Option<([u8; K], Pointer)>> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        let mut cursor = tx.cursor(&table)?;
        Ok(decode(cursor.first::<[u8; K], Encoded>()?))
    }

    pub fn last<const K: usize>(&self) -> Result<Option<([u8; K], Pointer)>> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        let mut cursor = tx.cursor(&table)?;
        Ok(decode(cursor.last::<[u8; K], Encoded>()?))
    }

    pub fn nearest_lower<const K: usize>(
        &self,
        key: &[u8; K],
    ) -> Result<Option<([u8; K], Pointer)>> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        let mut cursor = tx.cursor(&table)?;

        let found = match cursor.set_range::<[u8; K], Encoded>(key.as_slice())? {
            Some((found, value)) if &found == key => Some((found, value)),
            // When `set_range` returns `None` (the key exceeds every stored
            // key), MDBX leaves the cursor past the end so `prev` returns the
            // last entry. Pinned by `nearest_lower_above_all_keys`.
            _ => cursor.prev::<[u8; K], Encoded>()?,
        };

        Ok(decode(found))
    }

    pub fn nearest_upper<const K: usize>(
        &self,
        key: &[u8; K],
    ) -> Result<Option<([u8; K], Pointer)>> {
        let tx = self.db.begin_ro_txn()?;
        let table = tx.open_table(Some(TABLE))?;
        let mut cursor = tx.cursor(&table)?;
        Ok(decode(
            cursor.set_range::<[u8; K], Encoded>(key.as_slice())?,
        ))
    }
}

fn decode<const K: usize>(entry: Option<([u8; K], Encoded)>) -> Option<([u8; K], Pointer)> {
    entry.map(|(key, value)| (key, Pointer::from_le_bytes(value)))
}
