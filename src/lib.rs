//! Autumn plugin for RocksDB.

mod bounds;
mod client;
pub mod config;
mod envelope;
mod error;
mod metrics;
mod open;

pub use client::{Batch, Entry, Keyspace, Page, RocksDb, Scan};
pub use config::{AccessMode, Compression, ConfigError, RocksDbConfig};
pub use error::{RocksDbError, RocksDbResultExt};
pub use open::{CACHE_CF, Database, SESSIONS_CF, Setup};
/// The `rocksdb` crate that the plugin uses. Use it in setup hooks and in `with_db`.
pub use rocksdb;
