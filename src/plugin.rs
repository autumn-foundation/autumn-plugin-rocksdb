//! [`RocksDbPlugin`]: installs a [`RocksDb`] handle in an Autumn app.
//!
//! # Contract
//!
//! - `build` reads the configuration. A bad configuration stops the boot in the startup hook.
//! - With `sessions = true`, `build` installs a [`RocksSessionStore`]. The startup hook connects it
//!   and puts it in the app state. The TTL is `session_ttl_secs`, or else `session.max_age_secs` of Autumn.
//! - The startup hook opens the database on a blocking thread and puts the handle in the app state.
//! - With `cache = true`, the startup hook installs a [`RocksCache`] as the app cache.
//! - When Autumn marks the shutdown, a watch task flushes the database. The database stays open for the drain.
//! - The shutdown hook closes the database. Autumn runs it after the drain.
//! - The readiness check and the metrics source use the same handle.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use autumn_web::actuator::{MetricFamily, MetricsSource};
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::{AppState, AutumnError};

use crate::cache::RocksCache;
use crate::client::RocksDb;
use crate::config::{ConfigError, DEFAULT_SECTION, RocksDbConfig};
use crate::error::RocksDbError;
use crate::health::DatabaseCheck;
use crate::metrics::{Metrics, property_families};
use crate::open::{Database, Setup};
use crate::session::RocksSessionStore;

/// The plugin name in Autumn diagnostics.
pub const PLUGIN_NAME: &str = "autumn-plugin-rocksdb";

/// The interval of the shutdown watch.
const SHUTDOWN_WATCH: std::time::Duration = std::time::Duration::from_millis(200);

/// State that the plugin hooks share.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) handle: OnceLock<RocksDb>,
    pub(crate) metrics: Arc<Metrics>,
}

impl Shared {
    pub(crate) async fn shutdown(&self) {
        if let Some(db) = self.handle.get() {
            db.close().await;
        }
    }
}

/// The metrics source: counters and RocksDB properties.
struct Source(Arc<Shared>);

impl MetricsSource for Source {
    fn collect(&self) -> Vec<MetricFamily> {
        let mut families = self.0.metrics.families();
        if let Some(properties) = self.0.handle.get().and_then(RocksDb::properties) {
            families.extend(property_families(&properties));
        }
        families
    }
}

enum ConfigSource {
    Section(String),
    Explicit(Box<RocksDbConfig>),
}

type Change = Box<dyn FnOnce(&mut RocksDbConfig) + Send>;

/// Installs a [`RocksDb`] handle in an Autumn app.
///
/// ```rust,no_run
/// use autumn_plugin_rocksdb::RocksDbPlugin;
///
/// # async fn run() {
/// autumn_web::app().plugin(RocksDbPlugin::new()).run().await;
/// # }
/// ```
#[must_use]
pub struct RocksDbPlugin {
    source: ConfigSource,
    changes: Vec<Change>,
    setups: Vec<Setup>,
}

impl Default for RocksDbPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl RocksDbPlugin {
    /// Makes a plugin that reads `[rocksdb]`.
    pub fn new() -> Self {
        Self {
            source: ConfigSource::Section(DEFAULT_SECTION.to_owned()),
            changes: Vec::new(),
            setups: Vec::new(),
        }
    }

    /// Reads `[section]` instead of `[rocksdb]`.
    ///
    /// An app can have one RocksDB plugin only. Autumn ignores a second plugin with the same name.
    pub fn config_section(mut self, section: impl Into<String>) -> Self {
        self.source = ConfigSource::Section(section.into());
        self
    }

    /// Uses `config` and reads no files or variables.
    pub fn config(mut self, config: RocksDbConfig) -> Self {
        self.source = ConfigSource::Explicit(Box::new(config));
        self
    }

    /// Changes the configuration after the plugin reads it.
    pub fn configure(mut self, change: impl FnOnce(&mut RocksDbConfig) + Send + 'static) -> Self {
        self.changes.push(Box::new(change));
        self
    }

    /// Adds Rust code that runs on the database at startup, for example to seed data.
    ///
    /// Setup hooks run in order, before the first request. A failed hook stops the boot.
    pub fn setup(
        mut self,
        hook: impl Fn(&Database) -> Result<(), rocksdb::Error> + Send + Sync + 'static,
    ) -> Self {
        self.setups.push(Arc::new(hook));
        self
    }

    fn resolve(source: &ConfigSource, changes: Vec<Change>) -> Result<RocksDbConfig, ConfigError> {
        let mut config = match source {
            ConfigSource::Section(section) => RocksDbConfig::resolve(section)?,
            ConfigSource::Explicit(config) => (**config).clone(),
        };
        for change in changes {
            change(&mut config);
        }
        match source {
            ConfigSource::Section(section) => config.validate_section(section)?,
            ConfigSource::Explicit(_) => config.validate()?,
        }
        Ok(config)
    }
}

impl Plugin for RocksDbPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(PLUGIN_NAME)
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let Self {
            source,
            changes,
            setups,
        } = self;
        let mut app = app;
        if let ConfigSource::Section(section) = &source {
            app = app.config_section(section.clone());
        }
        let resolved = Self::resolve(&source, changes);
        let shared = Arc::new(Shared::default());
        app = app.metrics_source("rocksdb", Arc::new(Source(Arc::clone(&shared))));
        if resolved.as_ref().is_ok_and(|config| config.health_check) {
            app =
                app.health_indicator("rocksdb", Arc::new(DatabaseCheck::new(Arc::clone(&shared))));
        }
        let sessions = resolved
            .as_ref()
            .is_ok_and(|config| config.sessions)
            .then(RocksSessionStore::unbound);
        if let Some(store) = &sessions {
            app = app.with_session_store(store.clone());
        }
        let on_start = Arc::clone(&shared);
        let resolved = Arc::new(resolved);
        app.on_startup(move |state| {
            let shared = Arc::clone(&on_start);
            let resolved = Arc::clone(&resolved);
            let setups = setups.clone();
            let sessions = sessions.clone();
            async move {
                let config = resolved.as_ref().clone().map_err(|err| boot_error(&err))?;
                let db = RocksDb::open_with(config, setups, Arc::clone(&shared.metrics))
                    .await
                    .map_err(|err| boot_error(&err))?;
                state.insert_extension(db.clone());
                let _ = shared.handle.set(db.clone());
                tokio::spawn(watch_shutdown(state.clone(), db.clone()));
                if let Some(store) = sessions {
                    let max_age = state
                        .extension::<autumn_web::config::AutumnConfig>()
                        .map_or_else(
                            || autumn_web::session::SessionConfig::default().max_age_secs,
                            |config| config.session.max_age_secs,
                        );
                    let ttl = db.config().session_ttl(max_age);
                    store
                        .bind(db.clone(), ttl)
                        .map_err(|err| boot_error(&err))?;
                    state.insert_extension(store);
                }
                if db.config().cache {
                    let cache = RocksCache::new(db).map_err(|err| boot_error(&err))?;
                    if state.cache().is_some() {
                        tracing::warn!("RocksCache replaces the app cache that the app installed");
                    }
                    // Autumn keeps one cache for the whole process.
                    state.set_cache(Arc::new(cache));
                }
                tracing::info!("the RocksDB plugin is ready");
                Ok(())
            }
        })
        .on_shutdown(move || {
            let shared = Arc::clone(&shared);
            async move { shared.shutdown().await }
        })
    }
}

/// Flushes the database when Autumn marks the shutdown. The database stays open.
///
/// Autumn marks the shutdown before it drains the requests, so the handlers still need the database.
/// The drain can end the process before the shutdown hook closes the database. The flush keeps the data safe.
async fn watch_shutdown(state: AppState, db: RocksDb) {
    while !state.probes().is_shutting_down() {
        tokio::time::sleep(SHUTDOWN_WATCH).await;
    }
    match db.flush().await {
        Ok(()) => tracing::info!("the app shuts down: the RocksDB data is on disk"),
        Err(err) => tracing::warn!(error = %err, "the RocksDB flush at the shutdown mark failed"),
    }
}

/// A boot error. The text has no RocksDB message, because it can hold a path.
fn boot_error(err: &dyn std::fmt::Display) -> AutumnError {
    AutumnError::internal_server_error_msg(format!("{PLUGIN_NAME}: {err}"))
}

impl std::fmt::Debug for RocksDbPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            ConfigSource::Section(section) => section.as_str(),
            ConfigSource::Explicit(_) => "(explicit)",
        };
        f.debug_struct("RocksDbPlugin")
            .field("config", &source)
            .field("changes", &self.changes.len())
            .field("setups", &self.setups.len())
            .finish()
    }
}

impl RocksDb {
    /// Gets the handle from the app state, for example in a job or a task.
    #[must_use]
    pub fn from_state(state: &AppState) -> Option<Self> {
        state.extension::<Self>().map(|db| (*db).clone())
    }
}

impl axum::extract::FromRequestParts<AppState> for RocksDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Self::from_state(state).ok_or_else(|| RocksDbError::NotInstalled.into_autumn())
    }
}

#[cfg(test)]
mod tests;
