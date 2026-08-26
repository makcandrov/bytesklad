use std::io;

/// Errors returned by every fallible operation in this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("index error: {0}")]
    Index(#[from] libmdbx::Error),

    #[error("database is already open for writing by another process")]
    Locked,

    #[error("database does not exist or has not been initialized by a writer")]
    NotInitialized,

    #[error("key length mismatch: database was created with {stored}, opened with {requested}")]
    KeyLenMismatch { stored: usize, requested: usize },

    #[error(
        "segment size mismatch: database was created with {stored}, opened with {requested}; \
         the segment size is baked into stored pointers and cannot be changed"
    )]
    SegmentSizeMismatch { stored: u64, requested: u64 },

    #[error("a bucket record size must be greater than zero")]
    ZeroBucket,

    #[error("segment size must be greater than zero")]
    ZeroSegmentSize,

    #[error("too many buckets: at most {max} may be declared over the lifetime of a database")]
    TooManyBuckets { max: usize },

    #[error("store is full: logical offset exceeds the {max} byte addressable range")]
    StoreFull { max: u64 },

    #[error("index references bucket {0}, which is not present in the registry")]
    UnknownBucket(u8),

    #[error("corrupt {file}: {reason}")]
    Corrupt {
        file: &'static str,
        reason: &'static str,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn corrupt(file: &'static str, reason: &'static str) -> Self {
        Self::Corrupt { file, reason }
    }
}
