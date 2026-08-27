#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../README.md")]

use std::path::{Path, PathBuf};

use libmdbx::WriteFlags;

mod error;
pub use error::{Error, Result};

mod index;
use index::Index;

mod lock;
use lock::LockFile;

mod options;
pub use options::{DEFAULT_BUCKETS, DEFAULT_INDEX_MAP_SIZE, Options};

mod store;
use store::{Pointer, Store};

mod sys;

mod traits;
pub use traits::{DbRead, DbWrite};

mod varint;

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
/// Reads no configuration from the caller: the key length and the bucket
/// layout both come from the database itself. Sees every entry the writer has
/// committed, including data written to buckets that did not exist when this
/// handle was opened.
#[derive(Debug)]
pub struct DbRO<const K: usize> {
    path: PathBuf,
    index: Index,
    store: Store,
}

impl<const K: usize> DbRW<K> {
    /// Open an existing database for reading and writing.
    ///
    /// The whole stored configuration — key length and bucket layout — is read
    /// back from the database, so none of it is passed in.
    /// Fails with [`Error::NotInitialized`] if there is no database at `path`;
    /// use [`open_or_create`](Self::open_or_create) to make one.
    ///
    /// The index map size is not stored on disk, so this uses
    /// [`DEFAULT_INDEX_MAP_SIZE`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.is_dir() {
            return Err(Error::NotInitialized);
        }

        // Take the writer lock before touching anything else, so a second
        // writer cannot recover or truncate a store that is already in use.
        let lock = LockFile::acquire(&path)?;
        let store = Store::open_writable(&path, K)?;
        Self::finish(path, lock, store, DEFAULT_INDEX_MAP_SIZE)
    }

    /// Create the database at `path` with `options`, or open the one already
    /// there and require its configuration to match `options`.
    ///
    /// A database that does not exist is created with exactly this key length
    /// and bucket layout. One that does exist must already have them: a
    /// different key length fails with [`Error::KeyLenMismatch`] and a
    /// different set of buckets with [`Error::BucketsMismatch`]. Bucket
    /// *order* is not compared, since it only fixes internal tags.
    pub fn open_or_create(path: impl AsRef<Path>, options: &Options) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;

        let lock = LockFile::acquire(&path)?;
        let store = Store::create_writable(&path, K, options)?;
        Self::finish(path, lock, store, options.index_map_size)
    }

    fn finish(path: PathBuf, lock: LockFile, store: Store, index_map_size: u64) -> Result<Self> {
        let index = Index::open_writable(&path.join("index"), index_map_size)?;
        Ok(Self {
            path,
            index,
            store,
            _lock: lock,
        })
    }

    /// Record sizes of this database's buckets, in tag order.
    pub fn buckets(&self) -> &[usize] {
        self.store.bucket_sizes()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl<const K: usize> DbRO<K> {
    /// Open an existing database for reading.
    ///
    /// The whole stored configuration is read back from the database, so none
    /// of it is passed in. Fails with [`Error::NotInitialized`] if there is no
    /// database at `path`; use [`open_or_create`](Self::open_or_create) to
    /// make one.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Open the store first: it reports a missing or uninitialized database
        // as `NotInitialized`, which is clearer than the index's raw error.
        let store = Store::open_read_only(&path, K)?;
        let index = Index::open_read_only(&path.join("index"))?;

        Ok(Self { path, index, store })
    }

    /// Create the database at `path` with `options`, or open the one already
    /// there and require its configuration to match `options`.
    ///
    /// The read-only counterpart of [`DbRW::open_or_create`], with the same
    /// checks. Creating takes the writer lock for the length of the creation
    /// and releases it before returning, so what comes back is an ordinary
    /// reader. Useful when a reader may start before any writer has run.
    pub fn open_or_create(path: impl AsRef<Path>, options: &Options) -> Result<Self> {
        let path = path.as_ref();
        let db = match Self::open(path) {
            Err(Error::NotInitialized) => {
                // Creating is a write, so it goes through the writer handle,
                // which lays out the registry, the buckets and the index under
                // the exclusive lock and releases it again on drop. `Locked`
                // means another process got there first, and its creation
                // serves just as well.
                match DbRW::<K>::open_or_create(path, options) {
                    Ok(writer) => drop(writer),
                    Err(Error::Locked) => {}
                    Err(e) => return Err(e),
                }
                Self::open(path)?
            }
            result => result?,
        };

        db.store.check_config(options)?;
        Ok(db)
    }

    /// Record sizes of this database's buckets, in tag order.
    pub fn buckets(&self) -> &[usize] {
        self.store.bucket_sizes()
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
