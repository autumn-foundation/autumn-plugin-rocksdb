//! [`RocksCache`]: an Autumn `Cache` backend in the `autumn_cache` column family.
//!
//! # Contract
//!
//! - Values are JSON bytes in a TTL envelope. [`get_cached`](autumn_web::cache::get_cached) decodes them.
//! - `insert_value` stores [`RawCacheBytes`], `String`, `i64` and `i32` values. It ignores other types.
//! - `insert_raw_bytes` stores the bytes with the TTL. A read after the expiry is a miss.
//! - Each entry expires. `cache_ttl_secs` is the longest TTL and the TTL of an entry without one.
//! - `clear` removes each entry in one range delete. Cache keys are UTF-8, so no key has the byte `0xFF`.
//! - A key or value above the size limit is not stored. A read of such a key is a miss.
//! - Errors, a shutdown and bad envelopes give a miss or no write. The cache never panics and never logs keys.
//! - Each call takes a call slot without a wait. If no slot is free, a read is a miss and a write does nothing.
//! - Writes do not wait for a RocksDB write stall. A stalled write does nothing. Writes use `sync_writes`.
//! - On a multi-thread Tokio runtime, each call runs in `block_in_place`.
//! - The fill lock is not supported. One process owns the database.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::cache::{Cache, RawCacheBytes};
use rocksdb::{ColumnFamily, WriteOptions};
use tokio::runtime::RuntimeFlavor;

use crate::client::RocksDb;
use crate::envelope;
use crate::error::RocksDbError;
use crate::open::{CACHE_CF, Database};

/// An Autumn `Cache` backend that keeps entries in RocksDB.
///
/// Entries survive a restart of a file database. Set `cache = true` in the config to open the column family.
#[derive(Clone)]
pub struct RocksCache {
    db: RocksDb,
}

impl RocksCache {
    /// Makes a cache on `db`.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::UnknownColumnFamily`] if `cache` is off in the config,
    /// or [`RocksDbError::ReadOnly`] for a read-only database.
    pub fn new(db: RocksDb) -> Result<Self, RocksDbError> {
        if db.config().is_read_only() {
            return Err(RocksDbError::ReadOnly);
        }
        if !db.column_families().iter().any(|name| name == CACHE_CF) {
            return Err(RocksDbError::UnknownColumnFamily {
                name: CACHE_CF.to_owned(),
            });
        }
        Ok(Self { db })
    }

    /// Runs `work` with the cache column family. Errors and a shutdown give `None`.
    fn with_cf<T>(
        &self,
        work: impl FnOnce(&Database, &ColumnFamily) -> Result<T, rocksdb::Error>,
    ) -> Option<T> {
        let (_slot, db) = self.db.try_slot()?;
        let cf = db.cf_handle(CACHE_CF)?;
        match blocking(|| work(&db, cf)) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!(kind = ?err.kind(), "a RocksDB cache call failed");
                None
            }
        }
    }

    fn fits(&self, key: &str, value: &[u8]) -> bool {
        let config = self.db.config();
        key.len() <= config.max_key_bytes && value.len() <= config.max_value_bytes
    }

    /// The write options: no wait for a write stall, and `sync_writes`.
    fn write_options(&self) -> WriteOptions {
        let mut options = WriteOptions::default();
        options.set_no_slowdown(true);
        options.set_sync(self.db.config().sync_writes);
        options
    }

    fn store(&self, key: &str, bytes: &[u8], ttl: Option<Duration>) {
        if !self.fits(key, bytes) {
            return;
        }
        let longest = Duration::from_secs(self.db.config().cache_ttl_secs);
        let ttl = ttl.map_or(longest, |ttl| ttl.min(longest));
        let value = envelope::encode(bytes, Some(envelope::expiry(envelope::now_ms(), ttl)));
        let options = self.write_options();
        self.with_cf(|db, cf| db.put_cf_opt(cf, key, &value, &options));
    }

    fn load(&self, key: &str) -> Option<Vec<u8>> {
        if key.len() > self.db.config().max_key_bytes {
            return None;
        }
        let stored = self.with_cf(|db, cf| db.get_cf(cf, key)).flatten()?;
        let envelope = envelope::decode(&stored).ok()?;
        if envelope.is_expired(envelope::now_ms()) {
            return None;
        }
        Some(envelope.payload.to_vec())
    }
}

impl Cache for RocksCache {
    fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let found = self.load(key);
        if found.is_some() {
            self.db.metrics().cache_hit();
        } else {
            self.db.metrics().cache_miss();
        }
        found.map(|bytes| Arc::new(RawCacheBytes(bytes)) as Arc<dyn Any + Send + Sync>)
    }

    fn insert_value(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        let bytes = value.downcast_ref::<RawCacheBytes>().map_or_else(
            || {
                json(value.downcast_ref::<String>())
                    .or_else(|| json(value.downcast_ref::<i64>()))
                    .or_else(|| json(value.downcast_ref::<i32>()))
            },
            |raw| Some(raw.0.clone()),
        );
        if let Some(bytes) = bytes {
            self.store(key, &bytes, None);
        }
    }

    fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, ttl: Option<Duration>) {
        self.store(key, &bytes, ttl);
    }

    fn invalidate(&self, key: &str) {
        if key.len() <= self.db.config().max_key_bytes {
            let options = self.write_options();
            self.with_cf(|db, cf| db.delete_cf_opt(cf, key, &options));
        }
    }

    fn clear(&self) {
        // Cache keys are UTF-8. No UTF-8 text has the byte 0xFF, so this range holds each key.
        let options = self.write_options();
        self.with_cf(|db, cf| {
            db.delete_range_cf_opt(cf, [].as_slice(), [0xFF].as_slice(), &options)
        });
    }
}

/// Encodes a value of a known type as JSON.
fn json<T: serde::Serialize>(value: Option<&T>) -> Option<Vec<u8>> {
    serde_json::to_vec(value?).ok()
}

/// Runs `work` in `block_in_place` on a multi-thread runtime, or else on this thread.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

impl std::fmt::Debug for RocksCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksCache").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
