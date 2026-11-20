//! RocksDB fleets: one RocksDB database for each tenant or slot (feature `fleet`).
//!
//! # Contract
//!
//! [`RocksDbEngine`] is the RocksDB engine of an Autumn [`DatabaseFleet`]. Autumn core does the
//! keys, the paths, the opens, the eviction, the memory budget, the ownership and the fence.
//! The engine does the rest:
//!
//! - `open` opens the database directory with the engine configuration and runs the setup hooks.
//!   The reserved column family `autumn_fleet` holds the key name and the epoch.
//!   RocksDB lets one process open a database for writes, so the check and the stamp are atomic.
//! - `open` refuses a database of another key ([`FleetError::Misplaced`]) and a newer epoch
//!   ([`FleetError::NewerEpoch`]). It never lowers the stamp.
//! - `close` refuses new calls and flushes the memtables. RocksDB closes the database when the
//!   last open call ends.
//! - `snapshot` makes a RocksDB checkpoint, `dir/<directory name>`. A database with no write since
//!   the last snapshot gives the same files. A closed database is copied file by file, without
//!   `LOCK` and the info logs.
//! - `remove` deletes the database directory.
//! - The ship mode is [`ShipMode::Generations`].
//! - [`RocksDbFleetPlugin`] reads `[rocksdb_fleet]`, makes the fleet and puts it in the app state.
//!   [`FleetRocksDb`] is the extractor. It opens the database of the request tenant.
//! - With `[rocksdb_fleet.ownership]`, the plugin claims its slot ranges before the first open.
//! - The plugin closes idle databases, and closes all databases at shutdown. This runs in each role:
//!   a role that opens a database must also close it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::{DatabaseFleetConfig, Env};
use autumn_web::db::fleet::{DatabaseFleet, FleetError};
use autumn_web::db::fleet_engine::{FleetEngine, ShipMode};
use autumn_web::db::fleet_ownership::{FleetOwnership, OWNERSHIP_INDICATOR, OwnerId, SlotRange};
use autumn_web::fleet::FleetLease;
use autumn_web::fleet_layout::FleetDbKey;
use autumn_web::lock::FencingToken;
use autumn_web::plugin::Plugin;
use autumn_web::reexports::tokio_util::sync::CancellationToken;
use autumn_web::{AppState, AutumnError};
use rocksdb::{Options, WriteBatch, WriteOptions};

use crate::client::RocksDb;
use crate::config::{ConfigError, RocksDbConfig, layered_section};
use crate::error::RocksDbError;
use crate::metrics::Metrics;
use crate::open::{Database, FLEET_CF, Setup};

/// The plugin name of [`RocksDbFleetPlugin`] in Autumn diagnostics.
pub const FLEET_PLUGIN_NAME: &str = "autumn-plugin-rocksdb-fleet";

/// The default section of [`RocksDbFleetConfig`].
pub const DEFAULT_FLEET_SECTION: &str = "rocksdb_fleet";

/// The default interval between two shipped generations.
pub const DEFAULT_SHIP_EVERY: Duration = Duration::from_mins(1);

/// The default memory that the fleet counts for one open database: 128 MiB.
pub const DEFAULT_RESIDENT_BYTES: u64 = 128 * 1024 * 1024;

/// The identity key of the key name.
const NAME_KEY: &[u8] = b"name";

/// The identity key of the epoch, a big-endian `u64`.
const EPOCH_KEY: &[u8] = b"epoch";

/// Files that a copy of a closed database leaves out.
fn skipped(name: &str) -> bool {
    name == "LOCK" || name == "LOG" || name.starts_with("LOG.old")
}

/// The RocksDB engine of an Autumn fleet. See the module contract.
pub struct RocksDbEngine {
    template: RocksDbConfig,
    setups: Vec<Setup>,
    metrics: Arc<Metrics>,
    ship_every: Duration,
    resident_bytes: u64,
}

impl RocksDbEngine {
    /// Makes an engine that opens each database with `template`. The fleet sets the path.
    #[must_use]
    pub fn new(template: RocksDbConfig) -> Self {
        Self {
            template,
            setups: Vec::new(),
            metrics: Arc::default(),
            ship_every: DEFAULT_SHIP_EVERY,
            resident_bytes: DEFAULT_RESIDENT_BYTES,
        }
    }

    /// Sets the interval between two shipped generations.
    #[must_use]
    pub const fn ship_every(mut self, every: Duration) -> Self {
        self.ship_every = every;
        self
    }

    /// Sets the memory that the fleet counts for one open database.
    ///
    /// Set it near the block cache and the write buffers. The fleet budget closes idle databases with it.
    #[must_use]
    pub const fn resident_bytes(mut self, bytes: u64) -> Self {
        self.resident_bytes = bytes;
        self
    }

    /// Adds Rust code that runs on each database at open.
    #[must_use]
    pub fn setup(
        mut self,
        hook: impl Fn(&Database) -> Result<(), rocksdb::Error> + Send + Sync + 'static,
    ) -> Self {
        self.setups.push(Arc::new(hook));
        self
    }

    fn config_for(&self, path: &Path) -> RocksDbConfig {
        let mut config = self.template.clone();
        config.path = path.display().to_string();
        config.create_if_missing = true;
        config.cache = false;
        config.sessions = false;
        config.fleet_identity = true;
        config
    }
}

impl std::fmt::Debug for RocksDbEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDbEngine")
            .field("setups", &self.setups.len())
            .field("ship_every", &self.ship_every)
            .field("resident_bytes", &self.resident_bytes)
            .finish_non_exhaustive()
    }
}

/// Makes an I/O fleet error. The text has the error kind only, not the RocksDB message.
fn engine_error(op: &'static str) -> impl FnOnce(RocksDbError) -> FleetError {
    move |err| FleetError::Io {
        op,
        detail: err.to_string(),
    }
}

fn io_error(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |err| FleetError::Io {
        op,
        detail: err.to_string(),
    }
}

fn blocking_stopped(op: &'static str) -> FleetError {
    FleetError::Io {
        op,
        detail: "the blocking task stopped".to_owned(),
    }
}

/// Reads the stamp: the key name and the epoch.
fn read_identity(db: &Database) -> Result<Option<(String, u64)>, RocksDbError> {
    let Some(cf) = db.cf_handle(FLEET_CF) else {
        return Ok(None);
    };
    let Some(name) = db.get_cf(cf, NAME_KEY)? else {
        return Ok(None);
    };
    let epoch = db
        .get_cf(cf, EPOCH_KEY)?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
        .map_or(0, u64::from_be_bytes);
    Ok(Some((String::from_utf8_lossy(&name).into_owned(), epoch)))
}

/// Checks and writes the key and the epoch.
fn stamp(db: &Database, name: &str, epoch: u64) -> Result<Result<(), FleetError>, RocksDbError> {
    let cf = db
        .cf_handle(FLEET_CF)
        .ok_or_else(|| RocksDbError::UnknownColumnFamily {
            name: FLEET_CF.to_owned(),
        })?;
    let write = |with_name: bool| -> Result<(), RocksDbError> {
        let mut batch = WriteBatch::default();
        if with_name {
            batch.put_cf(cf, NAME_KEY, name.as_bytes());
        }
        batch.put_cf(cf, EPOCH_KEY, epoch.to_be_bytes());
        let mut options = WriteOptions::default();
        options.set_sync(true);
        db.write_opt(batch, &options)?;
        Ok(())
    };
    let Some((found, stamp)) = read_identity(db)? else {
        write(true)?;
        return Ok(Ok(()));
    };
    if found != name {
        return Ok(Err(FleetError::Misplaced {
            name: name.to_owned(),
            found,
        }));
    }
    if epoch > 0 && stamp > epoch {
        return Ok(Err(FleetError::NewerEpoch {
            name: name.to_owned(),
            epoch,
            found: stamp,
        }));
    }
    if epoch > stamp {
        write(false)?;
    }
    Ok(Ok(()))
}

/// Copies the files of the closed database `from` into the new directory `to`.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    crate::open::make_private(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if !entry.file_type()?.is_file() || skipped(&name.to_string_lossy()) {
            continue;
        }
        std::fs::copy(entry.path(), to.join(name))?;
    }
    Ok(())
}

/// Every file in `dir`.
fn files_in(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        files.push(entry?.path());
    }
    files.sort();
    Ok(files)
}

impl FleetEngine for RocksDbEngine {
    type Handle = RocksDb;

    async fn open(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: Option<FencingToken>,
    ) -> Result<RocksDb, FleetError> {
        let db = RocksDb::open_with(
            self.config_for(path),
            self.setups.clone(),
            Arc::clone(&self.metrics),
        )
        .await
        .map_err(engine_error("open database"))?;
        let name = key.name();
        let epoch = epoch.map_or(0, FencingToken::get);
        let stamped = db
            .internal(move |db| stamp(db, &name, epoch))
            .await
            .map_err(engine_error("stamp database"))
            .and_then(|stamped| stamped);
        if let Err(err) = stamped {
            db.close_now();
            return Err(err);
        }
        Ok(db)
    }

    fn close(&self, handle: &RocksDb) {
        handle.close_now();
    }

    fn in_use(&self, handle: &RocksDb) -> bool {
        handle.is_busy()
    }

    async fn snapshot(
        &self,
        handle: &RocksDb,
        path: &Path,
        dir: &Path,
    ) -> Result<Vec<PathBuf>, FleetError> {
        let name = path.file_name().ok_or_else(|| FleetError::Io {
            op: "snapshot database",
            detail: "the path has no file name".to_owned(),
        })?;
        let copy = dir.join(name);
        if handle.is_closed() {
            let (from, to) = (path.to_path_buf(), copy.clone());
            tokio::task::spawn_blocking(move || copy_dir(&from, &to))
                .await
                .map_err(|_| blocking_stopped("snapshot database"))?
                .map_err(io_error("snapshot database"))?;
        } else {
            handle
                .checkpoint(&copy)
                .await
                .map_err(engine_error("snapshot database"))?;
        }
        tokio::task::spawn_blocking(move || files_in(&copy))
            .await
            .map_err(|_| blocking_stopped("snapshot database"))?
            .map_err(io_error("snapshot database"))
    }

    fn remove(&self, _key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(io_error("delete database")(err)),
        }
    }

    fn resident_bytes(&self, _handle: &RocksDb) -> u64 {
        self.resident_bytes
    }

    fn ship_mode(&self) -> ShipMode {
        ShipMode::Generations {
            every: self.ship_every,
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.join("CURRENT").is_file()
    }

    fn epoch_stamp(&self, _key: &FleetDbKey, path: &Path) -> Result<Option<u64>, FleetError> {
        if !self.exists(path) {
            return Ok(None);
        }
        let read = || -> Result<Option<u64>, RocksDbError> {
            let options = Options::default();
            let names = Database::list_cf(&options, path)?;
            if !names.iter().any(|n| n == FLEET_CF) {
                return Ok(None);
            }
            let db = Database::open_cf_for_read_only(&options, path, names, false)?;
            Ok(read_identity(&db)?.map(|(_, epoch)| epoch))
        };
        read().map_err(engine_error("read epoch stamp"))
    }
}

/// The RocksDB database of the request tenant, from a [`RocksDbFleetPlugin`] fleet.
///
/// It derefs to [`RocksDb`]. The fleet keeps the database open while the value lives.
///
/// The rejection is the fleet error status: `400` with no tenant, `421` when another host owns the
/// slot, and `503` with no fleet.
#[derive(Debug, Clone)]
pub struct FleetRocksDb(pub FleetLease<RocksDbEngine>);

impl FleetRocksDb {
    /// Opens the database of `key` with no request, for example in a job or a task.
    ///
    /// # Errors
    ///
    /// Returns the [`FleetError`] of [`autumn_web::fleet::resolve_key`].
    pub async fn for_key(state: &AppState, key: &str) -> Result<Self, FleetError> {
        autumn_web::fleet::resolve_key::<RocksDbEngine>(state, key)
            .await
            .map(Self)
    }
}

impl std::ops::Deref for FleetRocksDb {
    type Target = RocksDb;

    fn deref(&self) -> &RocksDb {
        self.0.handle()
    }
}

impl axum::extract::FromRequestParts<AppState> for FleetRocksDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        autumn_web::fleet::resolve::<RocksDbEngine>(parts, state)
            .await
            .map(Self)
            .map_err(AutumnError::from)
    }
}

/// The configuration of a RocksDB fleet.
///
/// ```toml
/// [rocksdb_fleet]
/// mode = "tenant"
/// root = "data/tenants"
/// path = "{bucket}/{tenant}"
/// max_resident_bytes = 4294967296
/// ship_every_secs = 60
/// resident_bytes = 268435456
///
/// [rocksdb_fleet.engine]
/// block_cache_bytes = 33554432
/// write_buffer_bytes = 16777216
/// column_families = ["events"]
///
/// [rocksdb_fleet.ownership]
/// owner = "host-a"
/// ```
///
/// The top keys are the Autumn fleet keys (`[database.fleet]`). `engine` has the `[rocksdb]` keys,
/// except `path`, `cache` and `sessions`. The variables `AUTUMN_ROCKSDB_FLEET__ROOT` and
/// `AUTUMN_ROCKSDB_FLEET__OWNERSHIP__OWNER` set `root` and `ownership.owner`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RocksDbFleetConfig {
    /// The Autumn fleet keys.
    pub fleet: DatabaseFleetConfig,
    /// The RocksDB configuration of each database. The fleet sets the path.
    pub engine: RocksDbConfig,
    /// The interval between two shipped generations.
    pub ship_every: Duration,
    /// The memory that the fleet counts for one open database.
    pub resident_bytes: u64,
}

impl RocksDbFleetConfig {
    /// Makes a configuration with `fleet`, the default engine and the default limits.
    #[must_use]
    pub fn new(fleet: DatabaseFleetConfig) -> Self {
        Self {
            fleet,
            engine: RocksDbConfig::default(),
            ship_every: DEFAULT_SHIP_EVERY,
            resident_bytes: DEFAULT_RESIDENT_BYTES,
        }
    }

    /// Reads `[section]` from the app files and the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a file is not valid TOML or a value is not valid.
    pub fn resolve(section: &str) -> Result<Self, ConfigError> {
        autumn_web::dotenv::os_env_with_dotenv().map_or_else(
            |_| Self::resolve_with_env(section, &autumn_web::config::OsEnv),
            |env| Self::resolve_with_env(section, &env),
        )
    }

    /// Reads `[section]` with `env` as the environment.
    ///
    /// # Errors
    ///
    /// See [`resolve`](Self::resolve).
    pub fn resolve_with_env(section: &str, env: &dyn Env) -> Result<Self, ConfigError> {
        let mut table = layered_section(section, env)?;
        let prefix = env_prefix(section);
        if let Ok(root) = env.var(&format!("{prefix}__ROOT")) {
            table.insert("root".to_owned(), toml::Value::String(root));
        }
        if let Ok(owner) = env.var(&format!("{prefix}__OWNERSHIP__OWNER")) {
            let ownership = table
                .entry("ownership")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(ownership) = ownership {
                ownership.insert("owner".to_owned(), toml::Value::String(owner));
            }
        }
        Self::from_table(section, table)
    }

    /// Reads a configuration from the TOML table of `[section]`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for an unknown key or a value that is not valid.
    pub fn from_table(section: &str, mut table: toml::Table) -> Result<Self, ConfigError> {
        let bad = |key: &str, err: &dyn std::fmt::Display| {
            ConfigError(format!("[{section}] {key}: {err}"))
        };
        let engine = match table.remove("engine") {
            None => RocksDbConfig::default(),
            Some(toml::Value::Table(engine)) => {
                if engine.contains_key("path") {
                    return Err(ConfigError(format!(
                        "{section}.engine.path must not be set: the fleet sets the path"
                    )));
                }
                toml::Value::Table(engine)
                    .try_into()
                    .map_err(|err| bad("engine", &err))?
            }
            Some(_) => return Err(ConfigError(format!("{section}.engine must be a table"))),
        };
        let ship_every = match table.remove("ship_every_secs") {
            None => DEFAULT_SHIP_EVERY,
            Some(value) => Duration::from_secs(
                value
                    .as_integer()
                    .and_then(|v| u64::try_from(v).ok())
                    .ok_or_else(|| bad("ship_every_secs", &"must be a whole number of seconds"))?,
            ),
        };
        let resident_bytes = match table.remove("resident_bytes") {
            None => DEFAULT_RESIDENT_BYTES,
            Some(value) => value
                .as_integer()
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| bad("resident_bytes", &"must be a whole number"))?,
        };
        let fleet: DatabaseFleetConfig = toml::Value::Table(table)
            .try_into()
            .map_err(|err| bad("fleet", &err))?;
        let config = Self {
            fleet,
            engine,
            ship_every,
            resident_bytes,
        };
        config.validate_section(section)?;
        Ok(config)
    }

    /// Checks each value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] that names the first key that is not valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_section(DEFAULT_FLEET_SECTION)
    }

    fn validate_section(&self, section: &str) -> Result<(), ConfigError> {
        let fail = |key: &str, rule: &str| Err(ConfigError(format!("{section}.{key} {rule}")));
        if self.fleet.root.trim().is_empty() {
            return fail("root", "must be a directory");
        }
        if self.resident_bytes == 0 {
            return fail("resident_bytes", "must be 1 or more");
        }
        if self.ship_every.is_zero() {
            return fail("ship_every_secs", "must be 1 or more");
        }
        if self.engine.is_read_only() {
            return fail(
                "engine.access_mode",
                "must not be `read_only`: the fleet writes a stamp",
            );
        }
        if self.engine.cache || self.engine.sessions {
            return fail(
                "engine.cache",
                "and `engine.sessions` must be `false`: a fleet database is not the app cache",
            );
        }
        let mut engine = self.engine.clone();
        "fleet".clone_into(&mut engine.path);
        engine.validate_section(&format!("{section}.engine"))
    }

    /// Makes the engine of this configuration.
    #[must_use]
    pub fn engine(&self) -> RocksDbEngine {
        RocksDbEngine::new(self.engine.clone())
            .ship_every(self.ship_every)
            .resident_bytes(self.resident_bytes)
    }
}

fn env_prefix(section: &str) -> String {
    let name: String = section
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("AUTUMN_{name}")
}

enum FleetSource {
    Section(String),
    Explicit(Box<RocksDbFleetConfig>),
}

/// The running parts of a fleet: the idle sweeper and the ownership loop.
struct FleetRuntime {
    fleet: DatabaseFleet<RocksDbEngine>,
    stop: CancellationToken,
    maintenance: tokio::task::JoinHandle<()>,
    ownership: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl FleetRuntime {
    async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.maintenance.await;
        if let Some((stop, handle)) = self.ownership {
            stop.cancel();
            let _ = handle.await;
            if self.fleet.release_ownership().await {
                tracing::info!("the RocksDB fleet released its slot ranges");
            }
        }
    }
}

/// Makes a fleet of RocksDB databases and puts it in the app state.
///
/// ```rust,no_run
/// use autumn_plugin_rocksdb::fleet::{RocksDbFleetPlugin, FleetRocksDb};
///
/// #[autumn_web::get("/count")]
/// async fn count(db: FleetRocksDb) -> autumn_web::AutumnResult<String> {
///     let hits = db.get("hits").await.map_err(|e| e.into_autumn())?;
///     Ok(hits.map_or(0, |v| v.len()).to_string())
/// }
///
/// # async fn run() {
/// autumn_web::app()
///     .plugin(RocksDbFleetPlugin::new())
///     .routes(autumn_web::routes![count])
///     .run()
///     .await;
/// # }
/// ```
#[must_use]
pub struct RocksDbFleetPlugin {
    source: FleetSource,
    setups: Vec<Setup>,
}

impl Default for RocksDbFleetPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl RocksDbFleetPlugin {
    /// Makes a plugin that reads `[rocksdb_fleet]`.
    pub fn new() -> Self {
        Self {
            source: FleetSource::Section(DEFAULT_FLEET_SECTION.to_owned()),
            setups: Vec::new(),
        }
    }

    /// Reads `[section]` instead of `[rocksdb_fleet]`.
    pub fn config_section(mut self, section: impl Into<String>) -> Self {
        self.source = FleetSource::Section(section.into());
        self
    }

    /// Uses `config` and reads no files or variables.
    pub fn config(mut self, config: RocksDbFleetConfig) -> Self {
        self.source = FleetSource::Explicit(Box::new(config));
        self
    }

    /// Adds Rust code that runs on each database at open, for example to create tables.
    ///
    /// Setup hooks run in order, before the plugin disables external access and locks the configuration.
    pub fn setup(
        mut self,
        hook: impl Fn(&Database) -> Result<(), rocksdb::Error> + Send + Sync + 'static,
    ) -> Self {
        self.setups.push(Arc::new(hook));
        self
    }

    fn resolve(source: &FleetSource) -> Result<RocksDbFleetConfig, ConfigError> {
        match source {
            FleetSource::Section(section) => RocksDbFleetConfig::resolve(section),
            FleetSource::Explicit(config) => {
                config.validate()?;
                Ok((**config).clone())
            }
        }
    }
}

impl std::fmt::Debug for RocksDbFleetPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            FleetSource::Section(section) => section.as_str(),
            FleetSource::Explicit(_) => "(explicit)",
        };
        f.debug_struct("RocksDbFleetPlugin")
            .field("config", &source)
            .field("setups", &self.setups.len())
            .finish()
    }
}

/// Starts the fleet: ownership first, then the idle sweeper.
async fn start(
    state: &AppState,
    config: &RocksDbFleetConfig,
    engine: RocksDbEngine,
) -> Result<FleetRuntime, String> {
    let fleet = DatabaseFleet::with_engine(&config.fleet, engine).map_err(|e| e.to_string())?;
    let ownership = match &config.fleet.ownership {
        None => None,
        Some(settings) => {
            let store = autumn_web::fleet::ownership_store(state)
                .ok_or("fleet ownership needs a control database or a fleet_ownership_store")?;
            store.ensure_schema().await.map_err(|e| e.to_string())?;
            let ranges = settings
                .ranges()?
                .into_iter()
                .map(|(start, end)| SlotRange::new(start, end))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let owner = OwnerId::new(settings.owner.as_str()).map_err(|e| e.to_string())?;
            let ownership = Arc::new(
                FleetOwnership::new(
                    store,
                    owner,
                    ranges,
                    Duration::from_secs(settings.lease_secs),
                )
                .map_err(|e| e.to_string())?,
            );
            fleet
                .start_ownership(Arc::clone(&ownership))
                .await
                .map_err(|e| e.to_string())?;
            if let Err(err) = state.health_indicator_registry().register(
                OWNERSHIP_INDICATOR,
                autumn_web::actuator::IndicatorGroup::HealthOnly,
                ownership.indicator(),
            ) {
                tracing::warn!("{err}");
            }
            let stop = CancellationToken::new();
            let handle = fleet.spawn_ownership(stop.clone());
            Some((stop, handle))
        }
    };
    let stop = CancellationToken::new();
    let maintenance = fleet.spawn_maintenance(stop.clone());
    state.insert_extension(fleet.clone());
    Ok(FleetRuntime {
        fleet,
        stop,
        maintenance,
        ownership,
    })
}

impl Plugin for RocksDbFleetPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(FLEET_PLUGIN_NAME)
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let Self { source, setups } = self;
        let resolved = Arc::new(Self::resolve(&source));
        let runtime: Arc<Mutex<Option<FleetRuntime>>> = Arc::default();
        let on_start = Arc::clone(&runtime);
        app.on_startup(move |state| {
            let resolved = Arc::clone(&resolved);
            let runtime = Arc::clone(&on_start);
            let setups = setups.clone();
            async move {
                let fail = |err: &dyn std::fmt::Display| {
                    AutumnError::internal_server_error_msg(format!("{FLEET_PLUGIN_NAME}: {err}"))
                };
                let config = resolved.as_ref().clone().map_err(|err| fail(&err))?;
                let mut engine = config.engine();
                engine.setups = setups;
                let started = start(&state, &config, engine)
                    .await
                    .map_err(|err| fail(&err))?;
                *runtime.lock().unwrap_or_else(PoisonError::into_inner) = Some(started);
                tracing::info!("the RocksDB fleet is ready");
                Ok(())
            }
        })
        .on_shutdown(move || {
            let runtime = runtime
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            async move {
                if let Some(runtime) = runtime {
                    runtime.shutdown().await;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
