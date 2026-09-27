use std::io;

/// Errors returned by every fallible `bytesklad` operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("index error: {0}")]
    Index(#[from] libmdbx::Error),

    #[error("database is already open for writing by another handle")]
    Locked,

    #[error("database does not exist or has not been initialized by a writer")]
    NotInitialized,

    #[error("key length mismatch: database was created with {stored}, opened with {requested}")]
    KeyLenMismatch { stored: usize, requested: usize },

    #[error("bucket mismatch: database has buckets {stored:?}, opened with {requested:?}")]
    BucketsMismatch {
        stored: Vec<usize>,
        requested: Vec<usize>,
    },

    #[error(
        "a bucket record size must be greater than {max_inline}: \
         values that short are stored in the index itself"
    )]
    BucketTooSmall { max_inline: usize },

    #[error("a bucket record size must not exceed {max} bytes")]
    BucketTooLarge { max: u64 },

    #[error("too many buckets: a database has at most {max}")]
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

/// A `Result` alias whose error type is [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn corrupt(file: &'static str, reason: &'static str) -> Self {
        Self::Corrupt { file, reason }
    }
}
