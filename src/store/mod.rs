use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::{Error, Options, Result, sys};

mod bucket;
mod checkpoint;
mod file;
mod pointer;
mod registry;

use bucket::{Bucket, Kind};
use pointer::{INLINE_TAG, MAX_OFFSET, UNSIZED_TAG};
use registry::Registry;

pub(crate) use pointer::{MAX_INLINE_LEN, Pointer};

/// The byte store: every value ever inserted, laid out one file per bucket.
#[derive(Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    /// Buckets indexed by [`slot`]. Only ever appended to, so a tag always
    /// refers to the same bucket for the life of the database.
    buckets: RwLock<Vec<Arc<Bucket>>>,
    /// Record length to bucket tag, for routing writes.
    routing: FxHashMap<usize, u8>,
    bucket_sizes: Vec<usize>,
    /// Serializes `sync` so concurrent callers cannot interleave their fsyncs
    /// with the checkpoint write and publish a frontier for unsynced bytes.
    sync_lock: Mutex<()>,
    dirty: AtomicBool,
    writable: bool,
}

impl Store {
    /// Open an existing store for writing.
    pub fn open_writable(root: &Path, key_len: usize) -> Result<Self> {
        let dir = root.join("store");
        let registry = load_registry(&dir, key_len)?;
        Self::writable(dir, registry)
    }

    /// Create the store with `options`, or open the one already there and
    /// require its configuration to be exactly the one `options` describes.
    pub fn create_writable(root: &Path, key_len: usize, options: &Options) -> Result<Self> {
        options.validate()?;
        let dir = root.join("store");
        std::fs::create_dir_all(&dir)?;

        let registry = match Registry::load(&dir)? {
            Some(registry) => {
                check_key_len(&registry, key_len)?;
                check_config(&registry.buckets, options)?;
                registry
            }
            None => {
                let mut registry = Registry {
                    key_len,
                    buckets: Vec::new(),
                };
                registry.add_buckets(&options.buckets)?;
                registry.store(&dir)?;
                registry
            }
        };

        Self::writable(dir, registry)
    }

    fn writable(dir: PathBuf, registry: Registry) -> Result<Self> {
        let frontiers = checkpoint::load(&dir)?;
        let mut buckets = Vec::with_capacity(registry.buckets.len() + 1);
        for (tag, kind) in kinds(&registry) {
            buckets.push(Arc::new(Bucket::open_writable(
                bucket_path(&dir, kind),
                kind,
                frontiers.get(&tag).copied(),
            )?));
        }
        // The bucket files may have just been created; make their directory
        // entries durable before anything is written into them.
        sys::sync_dir(&dir)?;

        Ok(Self {
            dir,
            routing: routing(&registry),
            bucket_sizes: registry.buckets,
            buckets: RwLock::new(buckets),
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            writable: true,
        })
    }

    /// Open an existing store for reading.
    pub fn open_read_only(root: &Path, key_len: usize) -> Result<Self> {
        let dir = root.join("store");
        let registry = load_registry(&dir, key_len)?;

        Ok(Self {
            buckets: RwLock::new(read_only_buckets(&dir, &registry)),
            routing: routing(&registry),
            bucket_sizes: registry.buckets,
            dir,
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            writable: false,
        })
    }

    /// Fail unless this store's configuration is the one `options` describes.
    pub fn check_config(&self, options: &Options) -> Result<()> {
        options.validate()?;
        check_config(&self.bucket_sizes, options)
    }

    pub fn bucket_sizes(&self) -> &[usize] {
        &self.bucket_sizes
    }

    pub fn read(&self, pointer: Pointer) -> Result<Vec<u8>> {
        if pointer.tag() == INLINE_TAG {
            return pointer
                .inline_value()
                .ok_or_else(|| Error::corrupt("index", "inline length out of range"));
        }
        self.bucket(pointer.tag())?.read(pointer.offset())
    }

    pub fn append(&self, value: &[u8]) -> Result<Pointer> {
        // Short values live entirely in their pointer: no bytes are written,
        // so the store stays clean and nothing needs to be fsynced for them.
        // Checked before routing, which is why a size bucket at or below
        // `MAX_INLINE_LEN` is rejected at configuration time.
        if let Some(pointer) = Pointer::inline(value) {
            return Ok(pointer);
        }
        let tag = self
            .routing
            .get(&value.len())
            .copied()
            .unwrap_or(UNSIZED_TAG);
        let offset = self.bucket(tag)?.append(value)?;
        self.dirty.store(true, Ordering::SeqCst);
        Pointer::new(tag, offset).ok_or(Error::StoreFull { max: MAX_OFFSET })
    }

    /// Make every appended byte durable and publish the new frontier.
    ///
    /// Returns without touching the disk when nothing has been appended since
    /// the last call, and otherwise fsyncs only the buckets actually written.
    pub fn sync(&self) -> Result<()> {
        // The lock is taken before the flag is cleared so a caller that
        // appended and then called `sync` can never be told "already clean" by
        // a concurrent sync that had not yet flushed those bytes.
        let _guard = self.sync_lock.lock();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        let result = self.sync_inner();
        if result.is_err() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        result
    }

    fn sync_inner(&self) -> Result<()> {
        let buckets = self.buckets.read().clone();
        let mut frontiers = Vec::with_capacity(buckets.len());
        for (slot, bucket) in buckets.iter().enumerate() {
            frontiers.push((tag_of(slot), bucket.sync()?));
        }
        checkpoint::store(&self.dir, &frontiers)
    }

    fn bucket(&self, tag: u8) -> Result<Arc<Bucket>> {
        let slot = slot(tag);
        if let Some(bucket) = self.buckets.read().get(slot) {
            return Ok(bucket.clone());
        }
        if self.writable {
            return Err(Error::UnknownBucket(tag));
        }

        // A reader that started before the writer declared a new bucket will
        // meet its tag here. Re-read the registry rather than fail: buckets
        // are append-only, so reloading can only ever add entries.
        let mut guard = self.buckets.write();
        if guard.len() <= slot {
            let registry = Registry::load(&self.dir)?.ok_or(Error::NotInitialized)?;
            *guard = read_only_buckets(&self.dir, &registry);
        }
        guard.get(slot).cloned().ok_or(Error::UnknownBucket(tag))
    }
}

fn load_registry(dir: &Path, key_len: usize) -> Result<Registry> {
    let registry = Registry::load(dir)?.ok_or(Error::NotInitialized)?;
    check_key_len(&registry, key_len)?;
    Ok(registry)
}

/// Requires a stored configuration to be exactly the one `options` describes.
/// Bucket order is not compared: it only fixes tags, which are internal. Nor
/// are repeats, which `Options::buckets` being a public field allows and which
/// declare a single bucket just the same.
fn check_config(buckets: &[usize], options: &Options) -> Result<()> {
    let mut stored = buckets.to_vec();
    let mut requested = options.buckets.clone();
    stored.sort_unstable();
    requested.sort_unstable();
    requested.dedup();
    if stored != requested {
        return Err(Error::BucketsMismatch {
            stored: buckets.to_vec(),
            requested: options.buckets.clone(),
        });
    }
    Ok(())
}

fn check_key_len(registry: &Registry, key_len: usize) -> Result<()> {
    if registry.key_len != key_len {
        return Err(Error::KeyLenMismatch {
            stored: registry.key_len,
            requested: key_len,
        });
    }
    Ok(())
}

/// Position of `tag`'s bucket in `Store::buckets`, which holds the
/// variable-length bucket first and then the size buckets in tag order. Tags
/// are not usable as indices directly: the variable-length one is `255`, and
/// `INLINE_TAG` has no bucket at all and never reaches here.
fn slot(tag: u8) -> usize {
    match tag {
        UNSIZED_TAG => 0,
        tag => usize::from(tag),
    }
}

/// Inverse of [`slot`].
fn tag_of(slot: usize) -> u8 {
    match slot {
        0 => UNSIZED_TAG,
        slot => slot as u8,
    }
}

/// Tag `255` is the variable-length bucket; tag `i + 1` holds records of
/// `registry.buckets[i]` bytes.
fn kinds(registry: &Registry) -> Vec<(u8, Kind)> {
    std::iter::once((UNSIZED_TAG, Kind::Unsized))
        .chain(
            registry
                .buckets
                .iter()
                .enumerate()
                .map(|(i, &size)| ((i + 1) as u8, Kind::Sized(size))),
        )
        .collect()
}

fn routing(registry: &Registry) -> FxHashMap<usize, u8> {
    registry
        .buckets
        .iter()
        .enumerate()
        .map(|(i, &size)| (size, (i + 1) as u8))
        .collect()
}

fn read_only_buckets(dir: &Path, registry: &Registry) -> Vec<Arc<Bucket>> {
    kinds(registry)
        .into_iter()
        .map(|(_, kind)| Arc::new(Bucket::open_read_only(bucket_path(dir, kind), kind)))
        .collect()
}

/// A bucket's file is named after what it holds rather than after its tag: the
/// record size is the stable, meaningful half of the identity, and tags are
/// internal. Sizes are zero-padded so a listing comes out in size order.
fn bucket_path(dir: &Path, kind: Kind) -> PathBuf {
    match kind {
        Kind::Unsized => dir.join("unsized.bucket"),
        Kind::Sized(size) => dir.join(format!("sized-{size:010}.bucket")),
    }
}
