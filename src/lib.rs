#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../README.md")]

use std::path::{Path, PathBuf};

use libmdbx::WriteFlags;

mod error;
mod index;
mod lock;
mod options;
pub(crate) mod store;
mod sys;
mod traits;
mod varint;

pub use error::{Error, Result};
pub use options::{DEFAULT_INDEX_MAP_SIZE, Options};
pub use store::DEFAULT_SEGMENT_SIZE;
pub use traits::{DbRead, DbWrite};

use index::Index;
use lock::LockFile;
use store::{Pointer, Store};

// MDBX is impractical in a 2 GB address space, and the default map size does
// not fit in `isize` there either. Fail at compile time rather than at runtime.
const _: () = assert!(usize::BITS >= 64, "bytesklad requires a 64-bit target");

// A single writer shared across threads is a supported use case, and so is
// fanning readers out over a thread pool.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DbRW<32>>();
    assert_send_sync::<DbRO<32>>();
};

/// A database open for reading and writing.
///
/// Only one `DbRW` may exist for a path at a time, across all processes; the
/// exclusion is enforced by an advisory lock file and released if the process
/// dies. Any number of [`DbRO`] handles, in this or other processes, may read
/// concurrently with it.
///
/// The handle is `Sync`: `insert` and `insert_batch` may be called from
/// several threads at once.
#[derive(Debug)]
pub struct DbRW<const K: usize> {
    path: PathBuf,
    index: Index,
    store: Store,
    _lock: LockFile,
}

/// A database open for reading.
///
/// Reads no configuration from the caller: key length, segment size and the
/// bucket layout all come from the database itself. Sees every entry the
/// writer has committed, including data written to buckets and segments that
/// did not exist when this handle was opened.
#[derive(Debug)]
pub struct DbRO<const K: usize> {
    path: PathBuf,
    index: Index,
    store: Store,
}

impl<const K: usize> DbRW<K> {
    /// Open with the default [`Options`], creating the database if absent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Options::new().open(path)
    }

    pub(crate) fn open_with(path: impl AsRef<Path>, options: &Options) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;

        // Take the writer lock before touching anything else, so a second
        // writer cannot recover or truncate a store that is already in use.
        let lock = LockFile::acquire(&path)?;

        let store = Store::open_writable(&path, K, options)?;
        let index = Index::open_writable(&path.join("index"), options.index_map_size)?;

        Ok(Self {
            path,
            index,
            store,
            _lock: lock,
        })
    }

    /// Record sizes of the declared buckets, in tag order.
    pub fn buckets(&self) -> &[usize] {
        self.store.bucket_sizes()
    }

    /// The segment size this database was created with.
    pub fn segment_size(&self) -> u64 {
        self.store.segment_size()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl<const K: usize> DbRO<K> {
    /// Open an existing database for reading.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Options::new().open_read_only(path)
    }

    pub(crate) fn open_with(path: impl AsRef<Path>, _options: &Options) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Open the store first: it reports a missing or uninitialized database
        // as `NotInitialized`, which is clearer than the index's raw error.
        let store = Store::open_read_only(&path, K)?;
        let index = Index::open_read_only(&path.join("index"))?;

        Ok(Self { path, index, store })
    }

    /// Record sizes of the declared buckets, in tag order.
    pub fn buckets(&self) -> &[usize] {
        self.store.bucket_sizes()
    }

    /// The segment size this database was created with.
    pub fn segment_size(&self) -> u64 {
        self.store.segment_size()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

macro_rules! impl_read {
    ($ty:ident) => {
        impl<const K: usize> DbRead<K> for $ty<K> {
            fn get(&self, key: &[u8; K]) -> Result<Option<Vec<u8>>> {
                match self.index.get(key.as_slice())? {
                    Some(pointer) => self.store.read(pointer).map(Some),
                    None => Ok(None),
                }
            }

            fn contains(&self, key: &[u8; K]) -> Result<bool> {
                self.index.contains(key.as_slice())
            }

            fn len(&self) -> Result<usize> {
                self.index.len()
            }

            fn first(&self) -> Result<Option<([u8; K], Vec<u8>)>> {
                self.load(self.index.first::<K>()?)
            }

            fn last(&self) -> Result<Option<([u8; K], Vec<u8>)>> {
                self.load(self.index.last::<K>()?)
            }

            fn nearest_lower(&self, key: &[u8; K]) -> Result<Option<([u8; K], Vec<u8>)>> {
                self.load(self.index.nearest_lower(key)?)
            }

            fn nearest_upper(&self, key: &[u8; K]) -> Result<Option<([u8; K], Vec<u8>)>> {
                self.load(self.index.nearest_upper(key)?)
            }
        }

        impl<const K: usize> $ty<K> {
            fn load(
                &self,
                entry: Option<([u8; K], Pointer)>,
            ) -> Result<Option<([u8; K], Vec<u8>)>> {
                match entry {
                    Some((key, pointer)) => Ok(Some((key, self.store.read(pointer)?))),
                    None => Ok(None),
                }
            }
        }
    };
}

impl_read!(DbRW);
impl_read!(DbRO);

impl<const K: usize> DbWrite<K> for DbRW<K> {
    fn insert(&self, key: &[u8; K], value: &[u8]) -> Result<bool> {
        Ok(self.insert_batch(std::iter::once((key, value)))? == 1)
    }

    fn insert_batch<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a [u8; K], &'a [u8])>,
    ) -> Result<usize> {
        let tx = self.index.db().begin_rw_txn()?;
        let table = tx.open_table(Some(index::TABLE))?;

        let mut inserted = 0;
        for (key, value) in entries {
            if tx.get::<()>(&table, key.as_slice())?.is_some() {
                continue;
            }
            let pointer = self.store.append(value)?;
            tx.put(
                &table,
                key.as_slice(),
                pointer.to_le_bytes().as_slice(),
                WriteFlags::NO_OVERWRITE,
            )?;
            inserted += 1;
        }

        if inserted == 0 {
            return Ok(0);
        }

        // Order matters: the values must be on stable storage before the index
        // entries that name them become visible, or a crash could leave the
        // index pointing at bytes that were never written.
        self.store.sync()?;
        tx.commit()?;
        self.index.sync()?;

        Ok(inserted)
    }
}
