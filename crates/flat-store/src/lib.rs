mod file;
use file::DataFile;

mod lock;
use lock::LockFile;

mod store;
pub use store::{Error, FlatStore};
