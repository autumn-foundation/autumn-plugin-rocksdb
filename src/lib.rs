//! Autumn plugin for RocksDB.

mod bounds;
pub mod config;
mod envelope;
mod error;
mod open;

pub use config::{AccessMode, Compression, ConfigError, RocksDbConfig};
pub use error::{RocksDbError, RocksDbResultExt};
/// The `rocksdb` crate that the plugin uses. Use it in setup hooks and in `with_db`.
pub use rocksdb;
