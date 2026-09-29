//! The public error type.
//!
//! # Contract
//!
//! - A RocksDB error keeps its kind and its full message.
//! - The error text shows the kind only. [`RocksDbError::detail`] gives the full message.
//!   A RocksDB message can hold a file path. A JSON message can hold a value.
//! - A timeout gives HTTP 504. A shutdown and a busy database give 503.
//!   A key or value above the size limit gives 413. All other errors give 500.
//! - A timeout and the RocksDB kinds `Busy`, `TryAgain` and `TimedOut` are retryable.

use std::time::Duration;

use autumn_web::AutumnError;
use http::StatusCode;
use rocksdb::ErrorKind;

use crate::config::ConfigError;

/// An error from the plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RocksDbError {
    /// The configuration is not valid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// RocksDB refused the call.
    ///
    /// The text does not show `detail`, because it can hold a file path.
    #[error("RocksDB refused the call: {kind:?} error")]
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
    /// The scan page limit is 0 or larger than `max_scan_entries`.
    #[error("the scan limit {limit} is not from 1 to {max}")]
    #[non_exhaustive]
    ScanLimit {
        /// The requested limit.
        limit: usize,
        /// The largest limit.
        max: usize,
    },
    /// One entry has more bytes than `max_scan_bytes`. No page can hold it.
    #[error("one entry has more than {limit_bytes} bytes: read it with `get`")]
    #[non_exhaustive]
    EntryTooLarge {
        /// The byte limit of a page.
        limit_bytes: usize,
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

impl From<rocksdb::Error> for RocksDbError {
    fn from(err: rocksdb::Error) -> Self {
        Self::Database {
            kind: err.kind(),
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
    pub const fn kind(&self) -> Option<&ErrorKind> {
        match self {
            Self::Database { kind, .. } => Some(kind),
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
            Self::KeyTooLarge { .. } | Self::ValueTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
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
