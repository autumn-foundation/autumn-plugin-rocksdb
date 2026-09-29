//! [`RocksSessionStore`]: an Autumn `SessionStore` backend in the `autumn_sessions` column family.
//!
//! # Contract
//!
//! - A session is a JSON map in a TTL envelope. Each save sets the expiry to now plus the TTL.
//! - A load after the expiry gives `None`. The compaction filter removes the entry later.
//! - A load or destroy of an ID above the key size limit is not an error. A save of it gives an error.
//! - Data that does not decode gives `None`, so the user gets a new session.
//! - Before the plugin binds the store, and after the shutdown, each call gives an error.
//! - The key is the SHA-256 hash of the session ID. The files on disk do not hold the IDs.
//! - Errors and logs never show the session ID or the session data.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use autumn_web::session::{SessionStore, SessionStoreError};
use sha2::{Digest, Sha256};

use crate::client::RocksDb;
use crate::envelope;
use crate::error::RocksDbError;
use crate::open::SESSIONS_CF;

/// An Autumn `SessionStore` backend that keeps sessions in RocksDB.
///
/// Sessions survive a restart of a file database. Set `sessions = true` in the config to install it.
#[derive(Clone)]
pub struct RocksSessionStore {
    slot: Arc<OnceLock<Bound>>,
}

struct Bound {
    db: RocksDb,
    ttl: Duration,
}

impl RocksSessionStore {
    /// Makes a session store on `db`. Each session expires `ttl` after its last save.
    ///
    /// # Errors
    ///
    /// Returns [`RocksDbError::UnknownColumnFamily`] if `sessions` is off in the config,
    /// or [`RocksDbError::ReadOnly`] for a read-only database.
    pub fn new(db: RocksDb, ttl: Duration) -> Result<Self, RocksDbError> {
        let store = Self::unbound();
        store.bind(db, ttl)?;
        Ok(store)
    }

    /// Makes a store that fails until [`bind`](Self::bind).
    pub(crate) fn unbound() -> Self {
        Self {
            slot: Arc::new(OnceLock::new()),
        }
    }

    /// Connects the store to `db`. A second bind has no effect.
    pub(crate) fn bind(&self, db: RocksDb, ttl: Duration) -> Result<(), RocksDbError> {
        if db.config().is_read_only() {
            return Err(RocksDbError::ReadOnly);
        }
        if !db.column_families().iter().any(|name| name == SESSIONS_CF) {
            return Err(RocksDbError::UnknownColumnFamily {
                name: SESSIONS_CF.to_owned(),
            });
        }
        let _ = self.slot.set(Bound { db, ttl });
        Ok(())
    }

    fn bound(&self, operation: &'static str) -> Result<&Bound, SessionStoreError> {
        self.slot.get().ok_or_else(|| {
            SessionStoreError::backend(operation, "the RocksDB session store is not ready")
        })
    }
}

/// The key of a session: the SHA-256 hash of its ID.
pub(crate) fn session_key(id: &str) -> [u8; 32] {
    Sha256::digest(id.as_bytes()).into()
}

/// Refuses an ID above the key size limit. No ID has to be that long.
fn check_id(db: &RocksDb, id: &str) -> Result<(), RocksDbError> {
    let limit = db.config().max_key_bytes;
    if id.len() > limit {
        return Err(RocksDbError::KeyTooLarge {
            size: id.len(),
            limit,
        });
    }
    Ok(())
}

impl SessionStore for RocksSessionStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        let bound = self.bound("load")?;
        if check_id(&bound.db, id).is_err() {
            // No session can have this ID. A long cookie must not fail each request.
            return Ok(None);
        }
        let sessions = bound.db.internal_cf(SESSIONS_CF);
        let stored = match sessions.get(session_key(id)).await {
            Ok(stored) => stored,
            // No session can have this ID. A long cookie must not fail each request.
            Err(RocksDbError::KeyTooLarge { .. }) => None,
            Err(err) => return Err(SessionStoreError::backend("load", err)),
        };
        let Some(stored) = stored else {
            return Ok(None);
        };
        let Ok(envelope) = envelope::decode(&stored) else {
            tracing::warn!("a stored session is not a valid envelope");
            return Ok(None);
        };
        if envelope.is_expired(envelope::now_ms()) {
            return Ok(None);
        }
        Ok(serde_json::from_slice(envelope.payload).map_or_else(
            |_| {
                tracing::warn!("a stored session does not decode");
                None
            },
            Some,
        ))
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        let bound = self.bound("save")?;
        check_id(&bound.db, id).map_err(|err| SessionStoreError::backend("save", err))?;
        let payload = serde_json::to_vec(&data).map_err(|err| {
            SessionStoreError::backend("save", RocksDbError::json("encode", &err))
        })?;
        let expires_at = envelope::expiry(envelope::now_ms(), bound.ttl);
        bound
            .db
            .internal_cf(SESSIONS_CF)
            .put(
                session_key(id),
                envelope::encode(&payload, Some(expires_at)),
            )
            .await
            .map_err(|err| SessionStoreError::backend("save", err))
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        let bound = self.bound("destroy")?;
        if check_id(&bound.db, id).is_err() {
            return Ok(());
        }
        bound
            .db
            .internal_cf(SESSIONS_CF)
            .delete(session_key(id))
            .await
            .map_err(|err| SessionStoreError::backend("destroy", err))
    }
}

impl std::fmt::Debug for RocksSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksSessionStore")
            .field("bound", &self.slot.get().is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
