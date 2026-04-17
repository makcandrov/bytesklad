#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod checkpoint;
use checkpoint::Checkpoint;

mod file;
use file::{DataFileRO, DataFileRW};

mod lock;
use lock::LockFile;

mod store;
pub use store::{Error, FlatStoreRO, FlatStoreRW};

mod traits;
pub use traits::{FlatStoreRead, FlatStoreWrite};
