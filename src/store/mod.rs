use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::{Error, Options, Result};

mod bucket;
mod checkpoint;
mod pointer;
mod registry;
mod segment;

use bucket::{Bucket, Kind};
pub(crate) use pointer::Pointer;
use pointer::{MAX_OFFSET, UNSIZED_TAG};
use registry::Registry;

/// Default cap on the size of a single segment file.
pub const DEFAULT_SEGMENT_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// The byte store: every value ever inserted, laid out in buckets of segments.
#[derive(Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    segment_size: u64,
    /// Buckets indexed by tag. Only ever appended to, so a tag always refers
    /// to the same bucket for the life of the database.
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
    pub fn open_writable(root: &Path, key_len: usize, options: &Options) -> Result<Self> {
        let dir = root.join("store");
        std::fs::create_dir_all(&dir)?;

        let registry = match Registry::load(&dir)? {
            Some(mut registry) => {
                check_key_len(&registry, key_len)?;
                if let Some(requested) = options.segment_size
                    && requested != registry.segment_size
                {
                    return Err(Error::SegmentSizeMismatch {
                        stored: registry.segment_size,
                        requested,
                    });
                }
                // Declaring buckets on an existing database only ever adds:
                // existing tags keep their meaning and records already written
                // stay exactly where they are.
                if registry.add_buckets(&options.buckets)? {
                    registry.store(&dir)?;
                }
                registry
            }
            None => {
                let segment_size = options.segment_size.unwrap_or(DEFAULT_SEGMENT_SIZE);
                if segment_size == 0 {
                    return Err(Error::ZeroSegmentSize);
                }
                let mut registry = Registry {
                    key_len,
                    segment_size,
                    buckets: Vec::new(),
                };
                registry.add_buckets(&options.buckets)?;
                registry.store(&dir)?;
                registry
            }
        };

        let frontiers = checkpoint::load(&dir)?;
        let mut buckets = Vec::with_capacity(registry.buckets.len() + 1);
        for (tag, kind) in kinds(&registry) {
            buckets.push(Arc::new(Bucket::open_writable(
                bucket_dir(&dir, tag),
                kind,
                registry.segment_size,
                frontiers.get(&tag).copied(),
            )?));
        }

        Ok(Self {
            dir,
            segment_size: registry.segment_size,
            routing: routing(&registry),
            bucket_sizes: registry.buckets,
            buckets: RwLock::new(buckets),
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            writable: true,
        })
    }

    pub fn open_read_only(root: &Path, key_len: usize) -> Result<Self> {
        let dir = root.join("store");
        let registry = Registry::load(&dir)?.ok_or(Error::NotInitialized)?;
        check_key_len(&registry, key_len)?;

        Ok(Self {
            segment_size: registry.segment_size,
            buckets: RwLock::new(read_only_buckets(&dir, &registry)),
            routing: routing(&registry),
            bucket_sizes: registry.buckets,
            dir,
            sync_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            writable: false,
        })
    }

    pub fn segment_size(&self) -> u64 {
        self.segment_size
    }

    pub fn bucket_sizes(&self) -> &[usize] {
        &self.bucket_sizes
    }

    pub fn read(&self, pointer: Pointer) -> Result<Vec<u8>> {
        self.bucket(pointer.tag())?.read(pointer.offset())
    }

    pub fn append(&self, value: &[u8]) -> Result<Pointer> {
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
    /// the last call, and otherwise fsyncs only the segments actually written.
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
        for (tag, bucket) in buckets.iter().enumerate() {
            let (segment, len) = bucket.sync()?;
            frontiers.push((tag as u8, segment, len));
        }
        checkpoint::store(&self.dir, &frontiers)
    }

    fn bucket(&self, tag: u8) -> Result<Arc<Bucket>> {
        if let Some(bucket) = self.buckets.read().get(usize::from(tag)) {
            return Ok(bucket.clone());
        }
        if self.writable {
            return Err(Error::UnknownBucket(tag));
        }

        // A reader that started before the writer declared a new bucket will
        // meet its tag here. Re-read the registry rather than fail: buckets
        // are append-only, so reloading can only ever add entries.
        let mut guard = self.buckets.write();
        if guard.len() <= usize::from(tag) {
            let registry = Registry::load(&self.dir)?.ok_or(Error::NotInitialized)?;
            *guard = read_only_buckets(&self.dir, &registry);
        }
        guard
            .get(usize::from(tag))
            .cloned()
            .ok_or(Error::UnknownBucket(tag))
    }
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

/// Tag 0 is the variable-length bucket; tag `i + 1` holds records of
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
        .map(|(tag, kind)| {
            Arc::new(Bucket::open_read_only(
                bucket_dir(dir, tag),
                kind,
                registry.segment_size,
            ))
        })
        .collect()
}

fn bucket_dir(dir: &Path, tag: u8) -> PathBuf {
    dir.join(format!("b{tag:03}"))
}
