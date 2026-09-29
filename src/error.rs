//! The public error type.
//!
//! # Contract
//!
//! - A RocksDB error keeps its kind and its full message.
//! - The error text shows the kind only. [`RocksDbError::detail`] gives the full message.
//!   A RocksDB message can hold a file path. A JSON message can hold a value.
//! - [`ErrorKind`] is the plugin's own copy of the RocksDB error kind. A `rocksdb` update does not change it.
//! - A timeout gives HTTP 504. A shutdown and a busy database give 503. A bad scan limit gives 400.
//!   A key, value or batch above the size limit gives 413. All other errors give 500.
//! - The `Debug` output is the error text. It does not show the full message.
//! - A timeout and the RocksDB kinds `Busy`, `TryAgain` and `TimedOut` are retryable.

use std::time::Duration;

use autumn_web::AutumnError;
use http::StatusCode;

use crate::config::ConfigError;

/// An error from the plugin.
///
/// The `Debug` output is the error text. It does not show `detail`.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RocksDbError {
    /// The configuration is not valid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// RocksDB refused the call.
    ///
    /// The text does not show `detail`, because it can hold a file path.
    #[error("RocksDB refused the call: {kind}")]
    #[non_exhaustive]
    Database {
        /// The RocksDB error kind.
        kind: ErrorKind,
        /// The full RocksDB message.
        detail: String,
    },
    /// The column family is not in the configuration.
    #[error("the column family `{name}` is not open: add it to `column_families`")]
    #[non_exhaustive]
    UnknownColumnFamily {
        /// The column family name.
        name: String,
    },
    /// The plugin keeps column families with the `autumn_` prefix for itself.
    #[error("the column family `{name}` is reserved for the plugin")]
    #[non_exhaustive]
    ReservedColumnFamily {
        /// The column family name.
        name: String,
    },
    /// The key has more bytes than the limit.
    #[error("the key has {size} bytes: the limit is {limit}")]
    #[non_exhaustive]
    KeyTooLarge {
        /// The key size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The value has more bytes than the limit.
    #[error("the value has {size} bytes: the limit is {limit}")]
    #[non_exhaustive]
    ValueTooLarge {
        /// The value size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The keys and values of a batch have more bytes than the limit.
    #[error("the batch has {size} bytes: the limit is {limit}")]
    #[non_exhaustive]
    BatchTooLarge {
        /// The batch size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The scan page limit is 0 or larger than `max_scan_entries`.
    ///
    /// The status is HTTP 400, because the limit often comes from the request.
    #[error("the scan limit {limit} is not from 1 to {max}")]
    #[non_exhaustive]
    ScanLimit {
        /// The requested limit.
        limit: usize,
        /// The largest limit.
        max: usize,
    },
    /// One entry has more bytes than `max_scan_bytes`. No page can hold it.
    ///
    /// Read it with `get`, or give `key` to [`Scan::after`](crate::Scan::after) to skip it.
    /// The text does not show `key`.
    #[error("one entry has more than {limit_bytes} bytes: read it with `get`")]
    #[non_exhaustive]
    EntryTooLarge {
        /// The byte limit of a page.
        limit_bytes: usize,
        /// The key of the large entry.
        key: Vec<u8>,
    },
    /// The operation needs a database on disk.
    #[error("{operation} is not supported for an in-memory database")]
    #[non_exhaustive]
    NotSupported {
        /// The operation.
        operation: &'static str,
    },
    /// The database is read-only. The plugin refuses writes.
    #[error("the database is read-only")]
    ReadOnly,
    /// A value does not encode or decode as JSON.
    ///
    /// The text does not show `detail`, because it can hold a value.
    #[error("the value does not {action} as JSON")]
    #[non_exhaustive]
    Json {
        /// `encode` or `decode`.
        action: &'static str,
        /// The full `serde_json` message.
        detail: String,
    },
    /// The call did not complete in time.
    #[error("the call did not complete in {timeout:?}")]
    #[non_exhaustive]
    Timeout {
        /// The timeout.
        timeout: Duration,
    },
    /// The app shuts down. The plugin starts no new calls.
    #[error("the app shuts down: the RocksDB plugin starts no new calls")]
    ShuttingDown,
    /// The blocking task of a call stopped. For example, the closure of `with_db` panicked.
    #[error("the RocksDB task stopped before it gave a result")]
    TaskFailed,
    /// The app does not have the plugin.
    #[error("the RocksDB plugin is not installed: add `RocksDbPlugin` to the app")]
    NotInstalled,
}

/// The kind of a RocksDB error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// RocksDB did not find a file or a key.
    NotFound,
    /// The data on disk is not valid.
    Corruption,
    /// The build or the open mode does not support the operation.
    NotSupported,
    /// An argument or an option is not valid.
    InvalidArgument,
    /// The file system refused an operation.
    Io,
    /// A merge is in progress.
    MergeInProgress,
    /// The operation did not complete. For example, a write stall stopped a write that must not wait.
    Incomplete,
    /// RocksDB shuts down.
    ShutdownInProgress,
    /// A RocksDB time limit ended the operation.
    TimedOut,
    /// RocksDB stopped the operation.
    Aborted,
    /// A resource is busy.
    Busy,
    /// The operation expired.
    Expired,
    /// A retry can succeed.
    TryAgain,
    /// A compaction is too large.
    CompactionTooLarge,
    /// The column family was dropped.
    ColumnFamilyDropped,
    /// RocksDB gave a kind that the plugin does not know.
    Unknown,
}

impl From<rocksdb::ErrorKind> for ErrorKind {
    fn from(kind: rocksdb::ErrorKind) -> Self {
        use rocksdb::ErrorKind as Rocks;
        match kind {
            Rocks::NotFound => Self::NotFound,
            Rocks::Corruption => Self::Corruption,
            Rocks::NotSupported => Self::NotSupported,
            Rocks::InvalidArgument => Self::InvalidArgument,
            Rocks::IOError => Self::Io,
            Rocks::MergeInProgress => Self::MergeInProgress,
            Rocks::Incomplete => Self::Incomplete,
            Rocks::ShutdownInProgress => Self::ShutdownInProgress,
            Rocks::TimedOut => Self::TimedOut,
            Rocks::Aborted => Self::Aborted,
            Rocks::Busy => Self::Busy,
            Rocks::Expired => Self::Expired,
            Rocks::TryAgain => Self::TryAgain,
            Rocks::CompactionTooLarge => Self::CompactionTooLarge,
            Rocks::ColumnFamilyDropped => Self::ColumnFamilyDropped,
            Rocks::Unknown => Self::Unknown,
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "not found",
            Self::Corruption => "corruption",
            Self::NotSupported => "not supported",
            Self::InvalidArgument => "invalid argument",
            Self::Io => "I/O error",
            Self::MergeInProgress => "merge in progress",
            Self::Incomplete => "incomplete",
            Self::ShutdownInProgress => "shutdown in progress",
            Self::TimedOut => "timed out",
            Self::Aborted => "aborted",
            Self::Busy => "busy",
            Self::Expired => "expired",
            Self::TryAgain => "try again",
            Self::CompactionTooLarge => "compaction too large",
            Self::ColumnFamilyDropped => "column family dropped",
            Self::Unknown => "unknown error",
        })
    }
}

impl std::fmt::Debug for RocksDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The derived output shows `detail`. It can hold a file path or a value.
        f.debug_tuple("RocksDbError")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl From<rocksdb::Error> for RocksDbError {
    fn from(err: rocksdb::Error) -> Self {
        Self::Database {
            kind: ErrorKind::from(err.kind()),
            detail: err.into_string(),
        }
    }
}

impl RocksDbError {
    /// Makes a [`RocksDbError::Json`] error.
    pub(crate) fn json(action: &'static str, err: &serde_json::Error) -> Self {
        Self::Json {
            action,
            detail: err.to_string(),
        }
    }

    /// The full message of a [`RocksDbError::Database`] or [`RocksDbError::Json`] error.
    ///
    /// The message can hold a file path or a value. Do not show it to users.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Database { detail, .. } | Self::Json { detail, .. } => Some(detail),
            _ => None,
        }
    }

    /// The RocksDB error kind of a [`RocksDbError::Database`] error.
    #[must_use]
    pub const fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Database { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Returns `true` if a retry of the same call can succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::Database {
                    kind: ErrorKind::Busy | ErrorKind::TryAgain | ErrorKind::TimedOut,
                    ..
                }
        )
    }

    /// The HTTP status for this error.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::ShuttingDown
            | Self::Database {
                kind:
                    ErrorKind::Busy
                    | ErrorKind::TryAgain
                    | ErrorKind::TimedOut
                    | ErrorKind::ShutdownInProgress,
                ..
            } => StatusCode::SERVICE_UNAVAILABLE,
            Self::KeyTooLarge { .. } | Self::ValueTooLarge { .. } | Self::BatchTooLarge { .. } => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::ScanLimit { .. } => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Converts to an [`AutumnError`] with [`status`](Self::status).
    ///
    /// The `?` operator also converts, but always gives status 500.
    /// Autumn shows server error details only in development.
    #[must_use]
    pub fn into_autumn(self) -> AutumnError {
        let status = self.status();
        AutumnError::internal_server_error(self).with_status(status)
    }
}

/// Adds [`or_http`](RocksDbResultExt::or_http) to `Result<T, RocksDbError>`.
pub trait RocksDbResultExt<T> {
    /// Converts the error with [`RocksDbError::into_autumn`].
    ///
    /// # Errors
    ///
    /// Returns the converted error.
    fn or_http(self) -> Result<T, AutumnError>;
}

impl<T> RocksDbResultExt<T> for Result<T, RocksDbError> {
    fn or_http(self) -> Result<T, AutumnError> {
        self.map_err(RocksDbError::into_autumn)
    }
}

#[cfg(test)]
mod tests;
