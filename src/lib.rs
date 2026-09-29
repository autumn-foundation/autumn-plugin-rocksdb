//! Autumn plugin for RocksDB.

mod bounds;
mod cache;
mod client;
pub mod config;
mod envelope;
mod error;
mod health;
mod metrics;
mod open;
mod plugin;
mod session;

pub use cache::RocksCache;
pub use client::{Batch, Entry, Keyspace, Page, RocksDb, Scan};
pub use config::{AccessMode, Compression, ConfigError, RocksDbConfig};
pub use error::{RocksDbError, RocksDbResultExt};
pub use open::{CACHE_CF, Database, SESSIONS_CF, Setup};
pub use plugin::{PLUGIN_NAME, RocksDbPlugin};
/// The `rocksdb` crate that the plugin uses. Use it in setup hooks and in `with_db`.
pub use rocksdb;
pub use session::RocksSessionStore;
