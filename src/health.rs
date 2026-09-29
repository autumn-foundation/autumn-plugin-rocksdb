//! The readiness check.
//!
//! # Contract
//!
//! - The check is down before the plugin starts and after the shutdown.
//! - The check is down if RocksDB has background errors or stops writes.
//! - The check reads in-memory properties on a blocking thread. It does not wait for a call slot.
//! - The output says `in-memory` or `file` and the access mode. It never shows the path or a RocksDB message.

use std::collections::HashMap;
use std::sync::Arc;

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator};

use crate::error::RocksDbError;
use crate::plugin::Shared;

/// The future type of a health check.
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Reads the RocksDB health properties.
pub(crate) struct DatabaseCheck {
    shared: Arc<Shared>,
}

impl DatabaseCheck {
    pub(crate) const fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

/// Gives the reason for a down state, if any.
pub(crate) const fn problem(background_errors: u64, write_stopped: u64) -> Option<&'static str> {
    if background_errors > 0 {
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
                let errors = db.property("rocksdb.background-errors").await?;
                let stopped = db.property("rocksdb.is-write-stopped").await?;
                Ok::<_, RocksDbError>((errors.unwrap_or(0), stopped.unwrap_or(0)))
            };
            let state = match read.await {
                Ok((errors, stopped)) => problem(errors, stopped),
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
