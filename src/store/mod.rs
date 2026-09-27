use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use parking_lot::Mutex;

use crate::{Error, Options, Result, sys};

mod bucket;
mod checkpoint;
mod file;
mod pointer;
mod registry;

use bucket::{Bucket, Kind};
use pointer::{INLINE_TAG, UNSIZED_TAG};
use registry::{Layout, Registry};

pub(crate) use pointer::{MAX_BUCKET_LEN, MAX_INLINE_LEN, Pointer};

/// The byte store: every value too long to inline, laid out one file per bucket.
#[derive(Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    /// Buckets indexed by [`slot`], fixed at open like the layout they follow.
    buckets: Vec<Bucket>,
    layout: Layout,
    /// Serializes `sync` so concurrent callers cannot interleave their fsyncs
    /// with the checkpoint write and publish a frontier for unsynced bytes.
    sync_lock: Mutex<()>,
    dirty: AtomicBool,
}

impl Store {
    /// Open an existing store for writing.
    pub fn open_writable(root: &Path, key_len: usize) -> Result<Self> {
        let dir = root.join("store");
        let registry = Registry::load(&dir)?.ok_or(Error::NotInitialized)?;
        registry.check_key_len(key_len)?;
        remove_stale_temps(&dir)?;
        Self::writable(dir, registry.into_layout())
    }

    /// Create the store with `options`, or open the one already there and
    /// require its configuration to be exactly the one `options` describes.
    pub fn create_writable(root: &Path, key_len: usize, options: &Options) -> Result<Self> {
        options.validate()?;
        let dir = root.join("store");
        std::fs::create_dir_all(&dir)?;
        remove_stale_temps(&dir)?;

        let layout = match Registry::load(&dir)? {
            Some(registry) => {
                registry.check_key_len(key_len)?;
                let layout = registry.into_layout();
                layout.check(&options.buckets)?;
                layout
            }
            None => {
                // A missing identity file is not evidence that an existing
                // database is safe to reinitialize with new pointer meanings.
                if std::fs::read_dir(&dir)?.next().transpose()?.is_some()
                    || root.join("index").try_exists()?
                {
                    return Err(Error::corrupt(
                        "registry",
                        "missing from an existing database",
                    ));
                }
                Registry::create(&dir, key_len, &options.buckets)?.into_layout()
            }
        };

        Self::writable(dir, layout)
    }

    fn writable(dir: PathBuf, layout: Layout) -> Result<Self> {
        let frontiers = checkpoint::load(&dir)?;
        let kinds = layout.kinds().collect::<Vec<_>>();
        if !frontiers.is_empty()
            && (frontiers.len() != kinds.len()
                || kinds.iter().any(|(tag, _)| !frontiers.contains_key(tag)))
        {
            return Err(Error::corrupt(
                "checkpoint",
                "bucket table does not match registry",
            ));
        }
        let mut buckets = Vec::with_capacity(kinds.len());
        for &(tag, kind) in &kinds {
            buckets.push(Bucket::open_writable(
                bucket_path(&dir, kind),
                kind,
                frontiers.get(&tag).copied(),
            )?);
        }
        // The bucket files may have just been created; make their directory
        // entries durable before anything is written into them.
        sys::sync_dir(&dir)?;
        // Publish an initial frontier before the first append. A crash during
        // the first batch can then be recovered without treating missing
        // recovery metadata as permission to discard a nonempty store.
        if frontiers.is_empty() {
            let initial = kinds
                .into_iter()
                .map(|(tag, _)| (tag, 0))
                .collect::<Vec<_>>();
            checkpoint::store(&dir, &initial)?;
        }

        Ok(Self {
            dir,
            layout,
            buckets,
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
        })
    }

    /// Open an existing store for reading.
    pub fn open_read_only(root: &Path, key_len: usize, options: Option<&Options>) -> Result<Self> {
        let dir = root.join("store");
        let registry = Registry::load(&dir)?.ok_or(Error::NotInitialized)?;
        registry.check_key_len(key_len)?;
        let layout = registry.into_layout();
        if let Some(options) = options {
            options.validate()?;
            layout.check(&options.buckets)?;
        }

        let buckets = layout
            .kinds()
            .map(|(_, kind)| Bucket::open_read_only(bucket_path(&dir, kind), kind))
            .collect();

        Ok(Self {
            buckets,
            layout,
            dir,
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
        })
    }

    pub fn bucket_sizes(&self) -> impl ExactSizeIterator<Item = usize> {
        self.layout.sizes()
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
        let tag = self.layout.tag_for(value.len()).unwrap_or(UNSIZED_TAG);
        let offset = self.bucket(tag)?.append(value)?;
        self.dirty.store(true, Ordering::SeqCst);
        Pointer::new(tag, offset).ok_or(Error::StoreFull {
            max: MAX_BUCKET_LEN,
        })
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
        let mut frontiers = Vec::with_capacity(self.buckets.len());
        for (slot, bucket) in self.buckets.iter().enumerate() {
            frontiers.push((tag_of(slot), bucket.sync()?));
        }
        checkpoint::store(&self.dir, &frontiers)
    }

    fn bucket(&self, tag: u8) -> Result<&Bucket> {
        self.buckets.get(slot(tag)).ok_or(Error::UnknownBucket(tag))
    }
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

// Only called with the writer lock held, so no other process can be mid-write.
fn remove_stale_temps(dir: &Path) -> Result<()> {
    for file in [registry::FILE, checkpoint::FILE] {
        sys::remove_stale_temps(&dir.join(file))?;
    }
    Ok(())
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
