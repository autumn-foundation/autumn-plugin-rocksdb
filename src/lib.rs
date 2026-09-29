//! Autumn plugin for RocksDB.
//!
//! Add [`RocksDbPlugin`] to the app. Then use the [`RocksDb`] extractor in a handler.
//!
//! ```rust,no_run
//! use autumn_plugin_rocksdb::{RocksDb, RocksDbPlugin, RocksDbResultExt as _};
//! use autumn_web::prelude::*;
//!
//! #[derive(serde::Deserialize, serde::Serialize)]
//! struct Profile {
//!     name: String,
//! }
//!
//! #[get("/profiles/{id}")]
//! async fn profile(db: RocksDb, Path(id): Path<u64>) -> AutumnResult<Json<Profile>> {
//!     let profile = db
//!         .cf("profiles")
//!         .get_json::<Profile>(id.to_be_bytes())
//!         .await
//!         .or_http()?;
//!     profile.map(Json).ok_or_else(|| AutumnError::not_found_msg("no profile"))
//! }
//!
//! # async fn run() {
//! autumn_web::app()
//!     .plugin(RocksDbPlugin::new().configure(|c| {
//!         c.path = "data/rocksdb".into();
//!         c.column_families = vec!["profiles".into()];
//!     }))
//!     .routes(routes![profile])
//!     .run()
//!     .await;
//! # }
//! ```
//!
//! The plugin reads `[rocksdb]` in `autumn.toml`. See [`config`] for the keys.
//!
//! # What the plugin gives
//!
//! - [`RocksDb`] and [`Keyspace`]: reads, writes, JSON values, scans in pages and atomic batches.
//! - [`RocksCache`]: the Autumn app cache in RocksDB. Set `cache = true`. Each entry expires.
//! - [`RocksSessionStore`]: the Autumn session store in RocksDB. Set `sessions = true`.
//! - A readiness check, Prometheus metrics and a flush at shutdown.
//!
//! # Limits
//!
//! - Each call from async code runs on a blocking thread. A limit applies to the calls that run at the same time.
//! - Each call has a timeout. A timed-out call keeps its slot until RocksDB returns.
//! - A cache call does not wait for a slot. If no slot is free, a read is a miss and a write does nothing.
//! - Each key, value and batch has a size limit. Each scan page has an entry limit and a byte limit.
//! - Column families with the `autumn_` prefix belong to the plugin. The user API refuses them.
//! - Logs, error text and `Debug` output do not have keys, values, session IDs or RocksDB messages.
//!
//! # Shutdown
//!
//! Autumn marks the shutdown before it drains the requests. At the mark, the plugin flushes the database.
//! The database stays open for the drain. The shutdown hook closes it after the drain.

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
pub use error::{ErrorKind, RocksDbError, RocksDbResultExt};
pub use open::{CACHE_CF, Database, SESSIONS_CF};
pub use plugin::{PLUGIN_NAME, RocksDbPlugin};
/// The `rocksdb` crate that the plugin uses. Use it in setup hooks and in `with_db`.
pub use rocksdb;
pub use session::RocksSessionStore;
