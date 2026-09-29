//! [`RocksDb`], [`Keyspace`], [`Scan`] and [`Batch`]: calls with a deadline and limits.
//!
//! # Contract
//!
//! - Each call runs on a blocking thread. A semaphore limits the open calls to `max_concurrent_calls`.
//! - Each call has a deadline: `timeout_ms` from the call start. The wait for a call slot counts.
//! - At the deadline, the call gives [`RocksDbError::Timeout`] at once.
//!   A call that did not start never runs. RocksDB cannot stop a call that started.
//!   That call keeps its slot until RocksDB returns. Its outcome is unknown.
//! - The plugin checks each key and value against the size limits before the call.
//!   The metrics do not count a call that a check refuses.
//! - A read-only database refuses each write before the call.
//! - The user API refuses column families with the `autumn_` prefix and column families that are not open.
//! - A scan page has at most `limit` entries and at most `max_scan_bytes` key and value bytes.
//!   [`Page::next`] is the cursor of the next page. It is `None` only when no entry is left.
//! - [`RocksDb::flush`] writes the write-ahead log and the memtables to disk. The database stays open.
//! - After [`RocksDb::close`], new calls fail with [`RocksDbError::ShuttingDown`].
//!   The close waits up to 5 seconds for open calls.
//!   Then it flushes a writable file database, if `flush_on_shutdown` is `true`, and closes it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use rocksdb::{ColumnFamily, IteratorMode, ReadOptions, WriteBatch, WriteOptions};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{OnceCell, Semaphore};
use tokio::time::Instant;

use crate::bounds::{Bounds, Range};
use crate::config::{RESERVED_PREFIX, RocksDbConfig};
use crate::error::RocksDbError;
use crate::metrics::{ColumnFamilyProperties, Metrics, Outcome, Properties};
use crate::open::{self, Database, Setup};

/// The longest wait at shutdown for open calls to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// A handle to the database. Clones share the database.
///
/// Get it with the extractor in a handler, or with [`RocksDb::from_state`].
/// The methods of `RocksDb` use the `default` column family. Use [`RocksDb::cf`] for others.
#[derive(Clone)]
pub struct RocksDb {
    inner: Arc<Inner>,
}

struct Inner {
    db: RwLock<Option<Arc<Database>>>,
    column_families: Vec<String>,
    config: RocksDbConfig,
    permits: Arc<Semaphore>,
    metrics: Arc<Metrics>,
    shutting_down: AtomicBool,
    shutdown_done: OnceCell<()>,
}

/// A handle to one column family.
#[derive(Clone)]
pub struct Keyspace {
    db: RocksDb,
    name: Arc<str>,
    internal: bool,
}

/// One key and its value.
///
/// The `Debug` output shows the sizes, not the bytes.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// The key.
    pub key: Vec<u8>,
    /// The value.
    pub value: Vec<u8>,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("key_bytes", &self.key.len())
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

impl Entry {
    /// Decodes the value as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::Json`] if the value does not decode as `T`.
    pub fn value_as<T: DeserializeOwned>(&self) -> Result<T, RocksDbError> {
        decode_json(&self.value)
    }
}

/// One page of a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page {
    /// The entries in key order.
    pub entries: Vec<Entry>,
    /// The cursor of the next page. Give it to [`Scan::after`]. `None` means no entry is left.
    pub next: Option<Vec<u8>>,
}

/// A scan of one column family in key order.
#[must_use = "a scan does nothing until you call `fetch`"]
pub struct Scan {
    keyspace: Keyspace,
    range: Range,
    limit: Option<usize>,
}

/// One write of a batch.
enum Write {
    Put {
        cf: Arc<str>,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        cf: Arc<str>,
        key: Vec<u8>,
    },
}

/// An atomic group of writes. All writes succeed, or none do.
#[must_use = "a batch does nothing until you call `commit`"]
pub struct Batch {
    db: RocksDb,
    writes: Vec<Write>,
}

/// Counts one call. A dropped call counts as cancelled.
struct Guard<'a> {
    metrics: &'a Metrics,
    outcome: Option<Outcome>,
}

impl<'a> Guard<'a> {
    fn new(metrics: &'a Metrics) -> Self {
        metrics.started();
        Self {
            metrics,
            outcome: None,
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.metrics
            .ended(self.outcome.unwrap_or(Outcome::Cancelled));
    }
}

impl RocksDb {
    /// Opens a database without the plugin, for example in a test or a tool.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::Config`] for a bad configuration, or the RocksDB error of the open.
    pub async fn open(config: RocksDbConfig) -> Result<Self, RocksDbError> {
        Self::open_with(config, Vec::new(), Arc::default()).await
    }

    /// Opens the database with setup hooks and shared metrics.
    pub(crate) async fn open_with(
        config: RocksDbConfig,
        setups: Vec<Setup>,
        metrics: Arc<Metrics>,
    ) -> Result<Self, RocksDbError> {
        config.validate()?;
        let (opened, config) = tokio::task::spawn_blocking(move || {
            open::open(&config, &setups).map(|opened| (opened, config))
        })
        .await
        .map_err(|_| RocksDbError::TaskFailed)??;
        Ok(Self {
            inner: Arc::new(Inner {
                db: RwLock::new(Some(Arc::new(opened.db))),
                column_families: opened.column_families,
                permits: Arc::new(Semaphore::new(config.max_concurrent_calls)),
                config,
                metrics,
                shutting_down: AtomicBool::new(false),
                shutdown_done: OnceCell::new(),
            }),
        })
    }

    /// A handle to the column family `name`.
    ///
    /// Calls on the handle fail if the column family is not open or has the `autumn_` prefix.
    #[must_use]
    pub fn cf(&self, name: &str) -> Keyspace {
        Keyspace {
            db: self.clone(),
            name: Arc::from(name),
            internal: false,
        }
    }

    /// A handle to a reserved column family.
    pub(crate) fn internal_cf(&self, name: &'static str) -> Keyspace {
        Keyspace {
            db: self.clone(),
            name: Arc::from(name),
            internal: true,
        }
    }

    fn default_cf(&self) -> Keyspace {
        self.cf(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
    }

    /// Reads the value of `key` in the `default` column family.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::get`].
    pub async fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>, RocksDbError> {
        self.default_cf().get(key).await
    }

    /// Reads the value of `key` in the `default` column family and decodes it as JSON.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::get_json`].
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<T>, RocksDbError> {
        self.default_cf().get_json(key).await
    }

    /// Returns `true` if `key` has a value in the `default` column family.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::exists`].
    pub async fn exists(&self, key: impl AsRef<[u8]>) -> Result<bool, RocksDbError> {
        self.default_cf().exists(key).await
    }

    /// Writes `value` for `key` in the `default` column family.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::put`].
    pub async fn put(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<(), RocksDbError> {
        self.default_cf().put(key, value).await
    }

    /// Writes `value` as JSON for `key` in the `default` column family.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::put_json`].
    pub async fn put_json<T: Serialize + Sync + ?Sized>(
        &self,
        key: impl AsRef<[u8]>,
        value: &T,
    ) -> Result<(), RocksDbError> {
        self.default_cf().put_json(key, value).await
    }

    /// Removes `key` from the `default` column family.
    ///
    /// # Errors
    ///
    /// See [`Keyspace::delete`].
    pub async fn delete(&self, key: impl AsRef<[u8]>) -> Result<(), RocksDbError> {
        self.default_cf().delete(key).await
    }

    /// Starts a scan of the `default` column family.
    pub fn scan(&self) -> Scan {
        self.default_cf().scan()
    }

    /// Starts an atomic batch of writes.
    pub fn batch(&self) -> Batch {
        Batch {
            db: self.clone(),
            writes: Vec::new(),
        }
    }

    /// Runs `work` with the full `rocksdb` API, on a blocking thread.
    ///
    /// Use it for snapshots, properties or compaction. The timeout and the call limit apply.
    /// The size limits and the reserved names do not apply. Do not change `autumn_` column families.
    ///
    /// # Errors
    ///
    /// Returns the error of `work`, [`RocksDbError::Timeout`], or [`RocksDbError::TaskFailed`] if `work` panics.
    pub async fn with_db<T, F>(&self, work: F) -> Result<T, RocksDbError>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T, rocksdb::Error> + Send + 'static,
    {
        self.call(move |db| work(db).map_err(RocksDbError::from))
            .await
    }

    /// Makes a consistent copy of the database in `dir`, for example for a backup.
    ///
    /// `dir` must not exist. The copy uses hard links when it can.
    /// On Unix, only the owner can open `dir`. The copy holds all data, so keep it safe.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::NotSupported`] for an in-memory database, or the RocksDB error.
    pub async fn checkpoint(&self, dir: impl AsRef<Path>) -> Result<(), RocksDbError> {
        if self.inner.config.is_in_memory() {
            return Err(RocksDbError::NotSupported {
                operation: "checkpoint",
            });
        }
        let dir = dir.as_ref().to_path_buf();
        self.call(move |db| {
            rocksdb::checkpoint::Checkpoint::new(db)?.create_checkpoint(&dir)?;
            open::make_private(&dir).map_err(|err| RocksDbError::Database {
                kind: crate::error::ErrorKind::Io,
                detail: err.to_string(),
            })
        })
        .await
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &RocksDbConfig {
        &self.inner.config
    }

    /// The names of all open column families, `default` first. The list includes reserved ones.
    #[must_use]
    pub fn column_families(&self) -> &[String] {
        &self.inner.column_families
    }

    /// Refuses reserved names for the user API and names that are not open.
    fn check_name(&self, name: &str, internal: bool) -> Result<(), RocksDbError> {
        if !internal && name.starts_with(RESERVED_PREFIX) {
            return Err(RocksDbError::ReservedColumnFamily {
                name: name.to_owned(),
            });
        }
        if !self.inner.column_families.iter().any(|n| n == name) {
            return Err(RocksDbError::UnknownColumnFamily {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    fn check_key(&self, key: &[u8]) -> Result<(), RocksDbError> {
        let limit = self.inner.config.max_key_bytes;
        if key.len() > limit {
            return Err(RocksDbError::KeyTooLarge {
                size: key.len(),
                limit,
            });
        }
        Ok(())
    }

    fn check_value(&self, value: &[u8]) -> Result<(), RocksDbError> {
        let limit = self.inner.config.max_value_bytes;
        if value.len() > limit {
            return Err(RocksDbError::ValueTooLarge {
                size: value.len(),
                limit,
            });
        }
        Ok(())
    }

    /// The shared metrics.
    pub(crate) fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    /// The database, if the handle is not shut down. It does not use a call slot.
    pub(crate) fn database(&self) -> Option<Arc<Database>> {
        self.inner
            .db
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Takes a free call slot and the database without a wait. It gives `None` if all slots are busy.
    ///
    /// The sync cache uses it. A busy database gives a cache miss, not a blocked thread.
    pub(crate) fn try_slot(&self) -> Option<(tokio::sync::SemaphorePermit<'_>, Arc<Database>)> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let permit = self.inner.permits.try_acquire().ok()?;
        Some((permit, self.database()?))
    }

    /// Reads a database property on a blocking thread. It does not use a call slot.
    pub(crate) async fn property(
        &self,
        name: &'static str,
        timeout: Duration,
    ) -> Result<Option<u64>, RocksDbError> {
        let db = self.database().ok_or(RocksDbError::ShuttingDown)?;
        let task = tokio::task::spawn_blocking(move || db.property_int_value(name));
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => Ok(result?),
            Ok(Err(_)) => Err(RocksDbError::TaskFailed),
            Err(_) => Err(RocksDbError::Timeout { timeout }),
        }
    }

    /// Reads the properties of each column family. It runs on the caller thread.
    ///
    /// RocksDB keeps these properties in memory. A missing value gives `0`.
    pub(crate) fn properties(&self) -> Option<Properties> {
        let db = self.database()?;
        let read = |cf: &ColumnFamily, name: &str| {
            db.property_int_value_cf(cf, name)
                .ok()
                .flatten()
                .unwrap_or(0)
        };
        let column_families = self
            .inner
            .column_families
            .iter()
            .filter_map(|name| {
                let cf = db.cf_handle(name)?;
                Some(ColumnFamilyProperties {
                    name: name.clone(),
                    estimated_keys: read(cf, "rocksdb.estimate-num-keys"),
                    sst_files_bytes: read(cf, "rocksdb.total-sst-files-size"),
                    memtable_bytes: read(cf, "rocksdb.cur-size-all-mem-tables"),
                    pending_compaction_bytes: read(cf, "rocksdb.estimate-pending-compaction-bytes"),
                })
            })
            .collect();
        Some(Properties {
            background_errors: db
                .property_int_value("rocksdb.background-errors")
                .ok()
                .flatten()
                .unwrap_or(0),
            column_families,
        })
    }

    /// Writes the write-ahead log and the memtables of a writable file database to disk.
    ///
    /// The database stays open. The call does not use a call slot.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::ShuttingDown`] after [`close`](Self::close), or the RocksDB error.
    pub async fn flush(&self) -> Result<(), RocksDbError> {
        let db = self.database().ok_or(RocksDbError::ShuttingDown)?;
        if !self.needs_flush() {
            return Ok(());
        }
        let names = self.inner.column_families.clone();
        tokio::task::spawn_blocking(move || flush_all(&db, &names))
            .await
            .map_err(|_| RocksDbError::TaskFailed)?
            .map_err(RocksDbError::from)
    }

    /// Returns `true` for a writable file database.
    fn needs_flush(&self) -> bool {
        let config = &self.inner.config;
        !config.is_in_memory() && !config.is_read_only()
    }

    /// Refuses new calls, waits for open calls, flushes a writable file database and closes it.
    ///
    /// The plugin calls it after Autumn drains the requests. A second call waits for the first one to end.
    /// A call that is still open after 5 seconds keeps the database open until it ends.
    pub async fn close(&self) {
        let inner = &*self.inner;
        inner
            .shutdown_done
            .get_or_init(|| async {
                inner.shutting_down.store(true, Ordering::Release);
                let all = u32::try_from(inner.config.max_concurrent_calls).unwrap_or(u32::MAX);
                // Hold all slots until the database is taken. No call can start in between.
                let drained =
                    tokio::time::timeout(SHUTDOWN_WAIT, inner.permits.acquire_many(all)).await;
                if !matches!(drained, Ok(Ok(_))) {
                    tracing::warn!("open RocksDB calls did not end before the close");
                }
                inner.permits.close();
                let db = inner
                    .db
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                drop(drained);
                let Some(db) = db else {
                    return;
                };
                let flush = inner.config.flush_on_shutdown && self.needs_flush();
                let names = inner.column_families.clone();
                let task = tokio::task::spawn_blocking(move || {
                    let result = if flush {
                        flush_all(&db, &names)
                    } else {
                        Ok(())
                    };
                    // Only the last reference closes the database, off the async thread.
                    let closed = Arc::try_unwrap(db).is_ok();
                    (result, closed)
                });
                match task.await {
                    Ok((Err(err), _)) => {
                        tracing::warn!(kind = ?err.kind(), "the RocksDB flush at the close failed");
                    }
                    Ok((Ok(()), true)) => tracing::info!("the RocksDB database is closed"),
                    Ok((Ok(()), false)) => tracing::warn!(
                        "a RocksDB call is still open: the database closes when it ends"
                    ),
                    Err(_) => tracing::warn!("the RocksDB close task stopped"),
                }
            })
            .await;
    }

    /// Runs `work` on a blocking thread with a call slot and a deadline.
    async fn call<T, F>(&self, work: F) -> Result<T, RocksDbError>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T, RocksDbError> + Send + 'static,
    {
        let inner = &*self.inner;
        if inner.shutting_down.load(Ordering::Acquire) {
            return Err(RocksDbError::ShuttingDown);
        }
        let mut guard = Guard::new(&inner.metrics);
        let result = self.run(work).await;
        guard.outcome = Some(match &result {
            Ok(_) => Outcome::Succeeded,
            Err(RocksDbError::Timeout { .. }) => Outcome::TimedOut,
            Err(_) => Outcome::Failed,
        });
        result
    }

    async fn run<T, F>(&self, work: F) -> Result<T, RocksDbError>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T, RocksDbError> + Send + 'static,
    {
        let inner = &*self.inner;
        let timeout = inner.config.timeout();
        let deadline = Instant::now() + timeout;
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&inner.permits).acquire_owned())
            .await
            .map_err(|_| RocksDbError::Timeout { timeout })?
            .map_err(|_| RocksDbError::ShuttingDown)?;
        // A shutdown can start after the first check. It takes the database away.
        let db = self.database().ok_or(RocksDbError::ShuttingDown)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let skip = Arc::clone(&cancelled);
        // The permit moves into the task. The slot stays taken until RocksDB returns.
        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // A call that waited in the queue past its deadline does no work.
            if skip.load(Ordering::Acquire) {
                return Err(RocksDbError::Timeout { timeout });
            }
            work(&db)
        });
        match tokio::time::timeout_at(deadline, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RocksDbError::TaskFailed),
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                task.abort();
                Err(RocksDbError::Timeout { timeout })
            }
        }
    }
}

impl std::fmt::Debug for RocksDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDb")
            .field("column_families", &self.inner.column_families)
            .finish_non_exhaustive()
    }
}

impl Keyspace {
    /// The column family name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads the value of `key`.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError`] if the key is too large, the column family is not usable or RocksDB fails.
    pub async fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>, RocksDbError> {
        let key = key.as_ref().to_vec();
        self.check(&key)?;
        let key_len = key.len();
        let name = Arc::clone(&self.name);
        let value = self
            .db
            .call(move |db| Ok(db.get_cf(handle(db, &name)?, &key)?))
            .await?;
        if let Some(value) = &value {
            self.db.metrics().read(key_len.saturating_add(value.len()));
        }
        Ok(value)
    }

    /// Reads the value of `key` and decodes it as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::Json`] if the value does not decode, or an error of [`get`](Self::get).
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<T>, RocksDbError> {
        self.get(key)
            .await?
            .map(|value| decode_json(&value))
            .transpose()
    }

    /// Returns `true` if `key` has a value.
    ///
    /// # Errors
    ///
    /// See [`get`](Self::get).
    pub async fn exists(&self, key: impl AsRef<[u8]>) -> Result<bool, RocksDbError> {
        let key = key.as_ref().to_vec();
        self.check(&key)?;
        let name = Arc::clone(&self.name);
        self.db
            .call(move |db| Ok(db.get_pinned_cf(handle(db, &name)?, &key)?.is_some()))
            .await
    }

    /// Writes `value` for `key`.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError`] if a size limit applies, the database is read-only or RocksDB fails.
    pub async fn put(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<(), RocksDbError> {
        self.db
            .batch()
            .write(Write::Put {
                cf: Arc::clone(&self.name),
                key: key.as_ref().to_vec(),
                value: value.as_ref().to_vec(),
            })
            .commit_as(self.internal)
            .await
    }

    /// Writes `value` as JSON for `key`.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::Json`] if the value does not encode, or an error of [`put`](Self::put).
    pub async fn put_json<T: Serialize + Sync + ?Sized>(
        &self,
        key: impl AsRef<[u8]>,
        value: &T,
    ) -> Result<(), RocksDbError> {
        let value = serde_json::to_vec(value).map_err(|err| RocksDbError::json("encode", &err))?;
        self.put(key, value).await
    }

    /// Removes `key`. A missing key is not an error.
    ///
    /// # Errors
    ///
    /// See [`put`](Self::put).
    pub async fn delete(&self, key: impl AsRef<[u8]>) -> Result<(), RocksDbError> {
        self.db
            .batch()
            .write(Write::Delete {
                cf: Arc::clone(&self.name),
                key: key.as_ref().to_vec(),
            })
            .commit_as(self.internal)
            .await
    }

    /// Starts a scan.
    pub fn scan(&self) -> Scan {
        Scan {
            keyspace: self.clone(),
            range: Range::default(),
            limit: None,
        }
    }

    /// Checks the name and the key before a call.
    fn check(&self, key: &[u8]) -> Result<(), RocksDbError> {
        self.db.check_name(&self.name, self.internal)?;
        self.db.check_key(key)
    }
}

impl std::fmt::Debug for Keyspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyspace")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Scan {
    /// Reads only keys that start with `prefix`.
    pub fn prefix(mut self, prefix: impl AsRef<[u8]>) -> Self {
        self.range.prefix = Some(prefix.as_ref().to_vec());
        self
    }

    /// Reads only keys that are equal to or larger than `key`.
    pub fn start(mut self, key: impl AsRef<[u8]>) -> Self {
        self.range.start = Some(key.as_ref().to_vec());
        self
    }

    /// Reads only keys that are smaller than `key`.
    pub fn end(mut self, key: impl AsRef<[u8]>) -> Self {
        self.range.end = Some(key.as_ref().to_vec());
        self
    }

    /// Reads only keys that are larger than `cursor`. Use [`Page::next`] of the last page.
    pub fn after(mut self, cursor: impl AsRef<[u8]>) -> Self {
        self.range.after = Some(cursor.as_ref().to_vec());
        self
    }

    /// Reads at most `limit` entries. The default is `max_scan_entries`.
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Reads one page.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::ScanLimit`] for a bad limit, [`RocksDbError::EntryTooLarge`] if one entry
    /// is larger than `max_scan_bytes`, or another [`RocksDbError`].
    pub async fn fetch(self) -> Result<Page, RocksDbError> {
        let Self {
            keyspace,
            range,
            limit,
        } = self;
        let db = &keyspace.db;
        let max = db.config().max_scan_entries;
        let limit = limit.unwrap_or(max);
        if !(1..=max).contains(&limit) {
            return Err(RocksDbError::ScanLimit { limit, max });
        }
        db.check_name(&keyspace.name, keyspace.internal)?;
        for key in [&range.prefix, &range.start, &range.end, &range.after]
            .into_iter()
            .flatten()
        {
            db.check_key(key)?;
        }
        let bounds = Bounds::new(&range);
        if bounds.is_empty() {
            return Ok(Page::default());
        }
        let max_bytes = db.config().max_scan_bytes;
        let name = Arc::clone(&keyspace.name);
        let page = db
            .call(move |db| read_page(db, &name, bounds, limit, max_bytes))
            .await?;
        let bytes = page
            .entries
            .iter()
            .map(|e| e.key.len().saturating_add(e.value.len()))
            .fold(0_usize, usize::saturating_add);
        db.metrics().read(bytes);
        Ok(page)
    }
}

impl std::fmt::Debug for Scan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan")
            .field("column_family", &self.keyspace.name)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl Batch {
    /// Adds a write of `value` for `key` in the `default` column family.
    pub fn put(self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Self {
        self.put_in(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, key, value)
    }

    /// Adds a removal of `key` in the `default` column family.
    pub fn delete(self, key: impl AsRef<[u8]>) -> Self {
        self.delete_in(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, key)
    }

    fn write(mut self, write: Write) -> Self {
        self.writes.push(write);
        self
    }

    /// Adds a write of `value` for `key` in the column family `cf`.
    pub fn put_in(mut self, cf: &str, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Self {
        self.writes.push(Write::Put {
            cf: Arc::from(cf),
            key: key.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        });
        self
    }

    /// Adds a removal of `key` in the column family `cf`.
    pub fn delete_in(mut self, cf: &str, key: impl AsRef<[u8]>) -> Self {
        self.writes.push(Write::Delete {
            cf: Arc::from(cf),
            key: key.as_ref().to_vec(),
        });
        self
    }

    /// The number of writes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.writes.len()
    }

    /// Returns `true` if the batch has no writes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Writes all entries in one atomic write.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError`] if a check fails or RocksDB fails. Then the batch writes nothing.
    pub async fn commit(self) -> Result<(), RocksDbError> {
        self.commit_as(false).await
    }

    /// Writes all entries. `internal` allows the reserved column families.
    async fn commit_as(self, internal: bool) -> Result<(), RocksDbError> {
        let Self { db, writes } = self;
        if db.config().is_read_only() {
            return Err(RocksDbError::ReadOnly);
        }
        let mut bytes = 0_usize;
        for write in &writes {
            let (cf, key, value) = match write {
                Write::Put { cf, key, value } => (cf, key, Some(value)),
                Write::Delete { cf, key } => (cf, key, None),
            };
            db.check_name(cf, internal)?;
            db.check_key(key)?;
            bytes = bytes.saturating_add(key.len());
            if let Some(value) = value {
                db.check_value(value)?;
                bytes = bytes.saturating_add(value.len());
            }
        }
        let limit = db.config().max_batch_bytes;
        if bytes > limit {
            return Err(RocksDbError::BatchTooLarge { size: bytes, limit });
        }
        if writes.is_empty() {
            return if db.inner.shutting_down.load(Ordering::Acquire) {
                Err(RocksDbError::ShuttingDown)
            } else {
                Ok(())
            };
        }
        let sync = db.config().sync_writes;
        db.call(move |db| {
            let mut batch = WriteBatch::default();
            for write in &writes {
                match write {
                    Write::Put { cf, key, value } => batch.put_cf(handle(db, cf)?, key, value),
                    Write::Delete { cf, key } => batch.delete_cf(handle(db, cf)?, key),
                }
            }
            let mut options = WriteOptions::default();
            options.set_sync(sync);
            Ok(db.write_opt(batch, &options)?)
        })
        .await?;
        db.metrics().written(bytes);
        Ok(())
    }
}

impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batch")
            .field("writes", &self.writes.len())
            .finish_non_exhaustive()
    }
}

/// Gives the handle of an open column family.
fn handle<'a>(db: &'a Database, name: &str) -> Result<&'a ColumnFamily, RocksDbError> {
    db.cf_handle(name)
        .ok_or_else(|| RocksDbError::UnknownColumnFamily {
            name: name.to_owned(),
        })
}

/// Reads one scan page in the bounds.
fn read_page(
    db: &Database,
    name: &str,
    bounds: Bounds,
    limit: usize,
    max_bytes: usize,
) -> Result<Page, RocksDbError> {
    let mut options = ReadOptions::default();
    options.set_iterate_lower_bound(bounds.lower);
    if let Some(upper) = bounds.upper {
        options.set_iterate_upper_bound(upper);
    }
    let mut page = Page::default();
    let mut bytes = 0_usize;
    for item in db.iterator_cf_opt(handle(db, name)?, options, IteratorMode::Start) {
        let (key, value) = item?;
        let size = key.len().saturating_add(value.len());
        let full = page.entries.len() == limit || bytes.saturating_add(size) > max_bytes;
        if full {
            if page.entries.is_empty() {
                return Err(RocksDbError::EntryTooLarge {
                    limit_bytes: max_bytes,
                    key: key.into_vec(),
                });
            }
            page.next = page.entries.last().map(|entry| entry.key.clone());
            break;
        }
        bytes = bytes.saturating_add(size);
        page.entries.push(Entry {
            key: key.into_vec(),
            value: value.into_vec(),
        });
    }
    Ok(page)
}

/// Flushes the write-ahead log and the memtables of each column family.
fn flush_all(db: &Database, names: &[String]) -> Result<(), rocksdb::Error> {
    db.flush_wal(true)?;
    for name in names {
        if let Some(cf) = db.cf_handle(name) {
            db.flush_cf(cf)?;
        }
    }
    Ok(())
}

fn decode_json<T: DeserializeOwned>(value: &[u8]) -> Result<T, RocksDbError> {
    serde_json::from_slice(value).map_err(|err| RocksDbError::json("decode", &err))
}

#[cfg(test)]
mod tests;
