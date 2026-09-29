//! The readiness check.
//!
//! # Contract
//!
//! - The check is down before the plugin starts and after the shutdown.
//! - The check is down if RocksDB stops writes, or if the background error count grew in the last 60 seconds.
//!   RocksDB never lowers the count, so an old error that RocksDB recovered from does not keep the check down.
//! - The check reads in-memory properties on a blocking thread. It does not wait for a call slot.
//! - The check waits at most 1 second, so it answers before the Autumn timeout of 2 seconds.
//! - The output says `in-memory` or `file` and the access mode. It never shows the path or a RocksDB message.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator};

use crate::error::RocksDbError;
use crate::plugin::Shared;

/// The future type of a health check.
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Reads the RocksDB health properties.
pub(crate) struct DatabaseCheck {
    shared: Arc<Shared>,
    errors: Mutex<ErrorTracker>,
}

/// The longest wait of the check for RocksDB.
pub(crate) const CHECK_WAIT: Duration = Duration::from_secs(1);

/// A background error counts as recent for this time.
const ERROR_WINDOW: Duration = Duration::from_secs(60);

/// Finds out if the background error count grew recently.
#[derive(Debug, Default)]
pub(crate) struct ErrorTracker {
    last_count: u64,
    last_growth: Option<Instant>,
}

impl ErrorTracker {
    /// Records `count` at `now`. Returns `true` if the count grew in the last 60 seconds.
    pub(crate) fn observe(&mut self, count: u64, now: Instant) -> bool {
        if count > self.last_count {
            self.last_count = count;
            self.last_growth = Some(now);
        }
        self.last_growth
            .is_some_and(|at| now.saturating_duration_since(at) < ERROR_WINDOW)
    }
}

impl DatabaseCheck {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            errors: Mutex::new(ErrorTracker::default()),
        }
    }
}

/// Gives the reason for a down state, if any.
pub(crate) const fn problem(recent_errors: bool, write_stopped: u64) -> Option<&'static str> {
    if recent_errors {
        Some("background errors")
    } else if write_stopped > 0 {
        Some("writes stopped")
    } else {
        None
    }
}

impl HealthIndicator for DatabaseCheck {
    fn check(&self) -> BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            let Some(db) = self.shared.handle.get() else {
                return HealthCheckOutput::down()
                    .with_details(details(&[("state", "not started")]));
            };
            let config = db.config();
            let database = if config.is_in_memory() {
                "in-memory"
            } else {
                "file"
            };
            let access_mode = if config.is_read_only() {
                "read_only"
            } else {
                "read_write"
            };
            let read = async {
                let errors = db.property("rocksdb.background-errors", CHECK_WAIT).await?;
                let stopped = db.property("rocksdb.is-write-stopped", CHECK_WAIT).await?;
                Ok::<_, RocksDbError>((errors.unwrap_or(0), stopped.unwrap_or(0)))
            };
            let state = match read.await {
                Ok((errors, stopped)) => {
                    let recent = self
                        .errors
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .observe(errors, Instant::now());
                    problem(recent, stopped)
                }
                Err(RocksDbError::ShuttingDown) => Some("shut down"),
                Err(err) => {
                    tracing::warn!(error = %err, "the RocksDB readiness check failed");
                    Some("check failed")
                }
            };
            let mut fields = vec![("database", database), ("access_mode", access_mode)];
            match state {
                None => HealthCheckOutput::up().with_details(details(&fields)),
                Some(state) => {
                    fields.push(("state", state));
                    HealthCheckOutput::down().with_details(details(&fields))
                }
            }
        })
    }
}

fn details(fields: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
    fields
        .iter()
        .map(|(key, value)| ((*key).to_owned(), serde_json::Value::from(*value)))
        .collect()
}

#[cfg(test)]
mod tests;
