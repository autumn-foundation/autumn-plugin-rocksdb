//! The `[rocksdb]` section of `autumn.toml`.
//!
//! # Contract
//!
//! Each layer overrides the layers before it:
//!
//! 1. The defaults.
//! 2. `[rocksdb]` in `autumn.toml`.
//! 3. `[profile.<name>.rocksdb]` in `autumn.toml`.
//! 4. `[rocksdb]` in `autumn-<name>.toml`.
//! 5. `AUTUMN_ROCKSDB__<KEY>` variables. `AUTUMN_ROCKSDB__TIMEOUT_MS` sets `timeout_ms`.
//!    A list variable has comma-separated items.
//!
//! The result must pass [`RocksDbConfig::validate`]. Unknown keys are errors.
//!
//! ```toml
//! [rocksdb]
//! path = "data/rocksdb"
//! column_families = ["users", "orders"]
//! timeout_ms = 5000
//! max_concurrent_calls = 64
//! cache = true
//! sessions = true
//! compression = "zstd"
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::config::Env;
use serde::{Deserialize, Serialize};

/// The default section name.
pub const DEFAULT_SECTION: &str = "rocksdb";

/// The path of an in-memory database.
pub const IN_MEMORY: &str = ":memory:";

/// The prefix of the column families that the plugin keeps for itself.
pub const RESERVED_PREFIX: &str = "autumn_";

/// A configuration that is not valid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub(crate) String);

/// How the plugin opens the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AccessMode {
    /// Reads and writes. One process can open the database.
    #[default]
    ReadWrite,
    /// Reads only. Many processes can open the database.
    ReadOnly,
}

/// The block compression of new data files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Compression {
    /// No compression.
    None,
    /// LZ4. It needs the `lz4` feature.
    Lz4,
    /// Zstandard. It needs the `zstd` feature.
    Zstd,
    /// Snappy. It needs the `snappy` feature.
    Snappy,
}

impl Default for Compression {
    fn default() -> Self {
        if cfg!(feature = "lz4") {
            Self::Lz4
        } else {
            Self::None
        }
    }
}

impl Compression {
    /// Returns `true` if the crate features build this compression into RocksDB.
    #[must_use]
    pub const fn is_available(self) -> bool {
        match self {
            Self::None => true,
            Self::Lz4 => cfg!(feature = "lz4"),
            Self::Zstd => cfg!(feature = "zstd"),
            Self::Snappy => cfg!(feature = "snappy"),
        }
    }
}

/// The plugin settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools, reason = "each bool is one TOML key")]
pub struct RocksDbConfig {
    /// The database directory. `:memory:` gives an in-memory database.
    pub path: String,
    /// How the plugin opens the database.
    pub access_mode: AccessMode,
    /// If `true`, the plugin makes a missing database directory.
    pub create_if_missing: bool,
    /// The column families that the app uses. The plugin makes missing ones.
    pub column_families: Vec<String>,
    /// The call timeout in milliseconds. It includes the wait for a call slot.
    pub timeout_ms: u64,
    /// The most calls that can run at the same time.
    pub max_concurrent_calls: usize,
    /// The most bytes in one key.
    pub max_key_bytes: usize,
    /// The most bytes in one value.
    pub max_value_bytes: usize,
    /// The most entries in one scan page.
    pub max_scan_entries: usize,
    /// The most key and value bytes in one scan page.
    pub max_scan_bytes: usize,
    /// The most key and value bytes in one batch.
    pub max_batch_bytes: usize,
    /// If `true`, each write waits until the write-ahead log is on disk.
    pub sync_writes: bool,
    /// If `true`, the plugin installs [`RocksCache`](crate::RocksCache) as the app cache.
    pub cache: bool,
    /// If `true`, the plugin installs [`RocksSessionStore`](crate::RocksSessionStore) as the session store.
    pub sessions: bool,
    /// The session time to live in seconds. `None` uses `session.max_age_secs` of Autumn.
    pub session_ttl_secs: Option<u64>,
    /// The longest cache time to live in seconds. A cache entry without a TTL gets this TTL.
    pub cache_ttl_secs: u64,
    /// If `true`, the plugin adds a readiness check.
    pub health_check: bool,
    /// If `true`, the plugin flushes the write-ahead log and the memtables at shutdown.
    pub flush_on_shutdown: bool,
    /// The block cache size in bytes. `None` uses the RocksDB default.
    pub block_cache_bytes: Option<usize>,
    /// The memtable size in bytes. `None` uses the RocksDB default.
    pub write_buffer_bytes: Option<usize>,
    /// The most open data files. `-1` means no limit. `None` uses the RocksDB default.
    pub max_open_files: Option<i32>,
    /// The most background flush and compaction jobs. `None` uses the RocksDB default.
    pub max_background_jobs: Option<i32>,
    /// The block compression of new data files.
    pub compression: Compression,
    /// The bloom filter bits for each key. `0` turns the bloom filter off.
    pub bloom_filter_bits: u32,
}

impl Default for RocksDbConfig {
    fn default() -> Self {
        Self {
            path: IN_MEMORY.to_owned(),
            access_mode: AccessMode::ReadWrite,
            create_if_missing: true,
            column_families: Vec::new(),
            timeout_ms: 5_000,
            max_concurrent_calls: 64,
            max_key_bytes: 16 * 1024,
            max_value_bytes: 16 * 1024 * 1024,
            max_scan_entries: 1_000,
            max_scan_bytes: 16 * 1024 * 1024,
            max_batch_bytes: 64 * 1024 * 1024,
            sync_writes: false,
            cache: false,
            sessions: false,
            session_ttl_secs: None,
            cache_ttl_secs: 86_400,
            health_check: true,
            flush_on_shutdown: true,
            block_cache_bytes: None,
            write_buffer_bytes: None,
            max_open_files: None,
            max_background_jobs: None,
            compression: Compression::default(),
            bloom_filter_bits: 10,
        }
    }
}

/// The type of a configuration leaf, for environment values.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    Unsigned,
    Signed,
    Bool,
    List,
}

/// Each leaf key and its type.
const LEAVES: &[(&str, Kind)] = &[
    ("path", Kind::Text),
    ("access_mode", Kind::Text),
    ("create_if_missing", Kind::Bool),
    ("column_families", Kind::List),
    ("timeout_ms", Kind::Unsigned),
    ("max_concurrent_calls", Kind::Unsigned),
    ("max_key_bytes", Kind::Unsigned),
    ("max_value_bytes", Kind::Unsigned),
    ("max_scan_entries", Kind::Unsigned),
    ("max_scan_bytes", Kind::Unsigned),
    ("max_batch_bytes", Kind::Unsigned),
    ("sync_writes", Kind::Bool),
    ("cache", Kind::Bool),
    ("sessions", Kind::Bool),
    ("session_ttl_secs", Kind::Unsigned),
    ("cache_ttl_secs", Kind::Unsigned),
    ("health_check", Kind::Bool),
    ("flush_on_shutdown", Kind::Bool),
    ("block_cache_bytes", Kind::Unsigned),
    ("write_buffer_bytes", Kind::Unsigned),
    ("max_open_files", Kind::Signed),
    ("max_background_jobs", Kind::Signed),
    ("compression", Kind::Text),
    ("bloom_filter_bits", Kind::Unsigned),
];

/// The largest call timeout: one day.
const MAX_TIMEOUT_MS: u64 = 86_400_000;
/// The largest call limit.
const MAX_CALLS: usize = 1024;
/// The largest key limit: 1 MiB.
const MAX_KEY_BYTES: usize = 1 << 20;
/// The largest value limit: 1 GiB.
const MAX_VALUE_BYTES: usize = 1 << 30;
/// The largest scan page.
const MAX_SCAN_ENTRIES: usize = 100_000;
/// The longest column family name.
const MAX_NAME_BYTES: usize = 255;

impl RocksDbConfig {
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
        let (selected, profile) = active_profile(env);
        let mut merged = toml::Table::new();
        if let Some(base) = read_toml(&config_file("autumn.toml", env))? {
            merge_section(&mut merged, base.get(section), section)?;
            for name in inline_profile_names(&profile) {
                let inline = base
                    .get("profile")
                    .and_then(|p| p.get(name))
                    .and_then(|p| p.get(section));
                merge_section(&mut merged, inline, section)?;
            }
        }
        for name in autumn_web::config::profile_override_file_lookup_names(&profile, &selected) {
            if let Some(file) = read_toml(&config_file(&format!("autumn-{name}.toml"), env))? {
                merge_section(&mut merged, file.get(section), section)?;
                break;
            }
        }
        apply_env(&mut merged, section, env)?;
        let config: Self = toml::Value::Table(merged)
            .try_into()
            .map_err(|err| ConfigError(format!("[{section}]: {err}")))?;
        config.validate_section(section)?;
        Ok(config)
    }

    /// Checks each value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] that names the first key that is not valid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_section(DEFAULT_SECTION)
    }

    /// Checks each value. The errors name keys in `section`.
    pub(crate) fn validate_section(&self, section: &str) -> Result<(), ConfigError> {
        let fail = |key: &str, rule: &str| Err(ConfigError(format!("{section}.{key} {rule}")));
        if self.path.trim().is_empty() {
            return fail("path", "must be a directory path or `:memory:`");
        }
        if self.path.trim() != self.path {
            return fail("path", "must not start or end with white space");
        }
        if self.path.eq_ignore_ascii_case(IN_MEMORY) && !self.is_in_memory() {
            return fail(
                "path",
                "must be `:memory:` in lower case for an in-memory database",
            );
        }
        if self.is_read_only() && self.is_in_memory() {
            return fail("access_mode", "must not be `read_only` for `:memory:`");
        }
        if self.is_read_only() && self.cache {
            return fail("cache", "must be `false` for a read-only database");
        }
        if self.is_read_only() && self.sessions {
            return fail("sessions", "must be `false` for a read-only database");
        }
        if let Some(rule) = self.column_family_rule() {
            return fail("column_families", &rule);
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return fail("timeout_ms", "must be from 1 to 86400000 (one day)");
        }
        if !(1..=MAX_CALLS).contains(&self.max_concurrent_calls) {
            return fail("max_concurrent_calls", "must be from 1 to 1024");
        }
        if !(1..=MAX_KEY_BYTES).contains(&self.max_key_bytes) {
            return fail("max_key_bytes", "must be from 1 to 1048576 (1 MiB)");
        }
        if !(1..=MAX_VALUE_BYTES).contains(&self.max_value_bytes) {
            return fail("max_value_bytes", "must be from 1 to 1073741824 (1 GiB)");
        }
        if !(1..=MAX_SCAN_ENTRIES).contains(&self.max_scan_entries) {
            return fail("max_scan_entries", "must be from 1 to 100000");
        }
        if self.max_scan_bytes == 0 {
            return fail("max_scan_bytes", "must be 1 or more");
        }
        if self.max_batch_bytes == 0 {
            return fail("max_batch_bytes", "must be 1 or more");
        }
        if self.session_ttl_secs == Some(0) {
            return fail("session_ttl_secs", "must be 1 or more");
        }
        if self.cache_ttl_secs == 0 {
            return fail("cache_ttl_secs", "must be 1 or more");
        }
        if self.block_cache_bytes == Some(0) {
            return fail("block_cache_bytes", "must be 1 or more");
        }
        if self.write_buffer_bytes == Some(0) {
            return fail("write_buffer_bytes", "must be 1 or more");
        }
        if self.max_open_files.is_some_and(|v| v != -1 && v < 1) {
            return fail("max_open_files", "must be -1 (no limit) or 1 or more");
        }
        if self
            .max_background_jobs
            .is_some_and(|v| !(1..=256).contains(&v))
        {
            return fail("max_background_jobs", "must be from 1 to 256");
        }
        if !self.compression.is_available() {
            return fail("compression", "needs the crate feature of the same name");
        }
        if self.bloom_filter_bits > 64 {
            return fail("bloom_filter_bits", "must be from 0 to 64");
        }
        Ok(())
    }

    /// Gives the broken rule of `column_families`, if any.
    fn column_family_rule(&self) -> Option<String> {
        for (index, name) in self.column_families.iter().enumerate() {
            let name_ok = !name.is_empty()
                && name.len() <= MAX_NAME_BYTES
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
            if !name_ok {
                return Some("names must be 1 to 255 of `A-Z a-z 0-9 _ - .`".to_owned());
            }
            if name == rocksdb::DEFAULT_COLUMN_FAMILY_NAME {
                return Some("must not have `default`: the plugin opens it".to_owned());
            }
            if name.starts_with(RESERVED_PREFIX) {
                return Some(format!(
                    "must not have `{name}`: `{RESERVED_PREFIX}` is reserved"
                ));
            }
            if self.column_families[..index].contains(name) {
                return Some(format!("must not have `{name}` two times"));
            }
        }
        None
    }

    /// The call timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Returns `true` for an in-memory database.
    #[must_use]
    pub fn is_in_memory(&self) -> bool {
        self.path == IN_MEMORY
    }

    /// Returns `true` for a read-only database.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.access_mode == AccessMode::ReadOnly
    }

    /// The session time to live. `autumn_max_age_secs` is `session.max_age_secs` of Autumn.
    #[must_use]
    pub(crate) fn session_ttl(&self, autumn_max_age_secs: u64) -> Duration {
        Duration::from_secs(self.session_ttl_secs.unwrap_or(autumn_max_age_secs).max(1))
    }
}

/// Gives the selected profile text and the normalized profile, as Autumn does.
fn active_profile(env: &dyn Env) -> (String, String) {
    let selected = ["AUTUMN_ENV", "AUTUMN_PROFILE"]
        .iter()
        .filter_map(|key| env.var(key).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| {
            let release = env.var("AUTUMN_IS_DEBUG").is_ok_and(|v| v == "0");
            if release { "prod" } else { "dev" }.to_owned()
        });
    let profile =
        autumn_web::config::normalize_profile_name(&selected).unwrap_or_else(|| "dev".to_owned());
    (selected, profile)
}

/// The inline profile names to read, in order. The canonical name is last.
fn inline_profile_names(profile: &str) -> Vec<&str> {
    match profile {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Finds a config file in `AUTUMN_MANIFEST_DIR`, or else in the working directory.
fn config_file(name: &str, env: &dyn Env) -> PathBuf {
    env.var("AUTUMN_MANIFEST_DIR")
        .ok()
        .map(|dir| Path::new(&dir).join(name))
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn read_toml(path: &Path) -> Result<Option<toml::Table>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map(Some)
            .map_err(|err| ConfigError(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ConfigError(format!("{}: {err}", path.display()))),
    }
}

fn merge_section(
    into: &mut toml::Table,
    layer: Option<&toml::Value>,
    section: &str,
) -> Result<(), ConfigError> {
    match layer {
        None => Ok(()),
        Some(toml::Value::Table(table)) => {
            deep_merge(into, table);
            Ok(())
        }
        Some(_) => Err(ConfigError(format!("[{section}] must be a table"))),
    }
}

fn deep_merge(into: &mut toml::Table, layer: &toml::Table) {
    for (key, value) in layer {
        match (into.get_mut(key), value) {
            (Some(toml::Value::Table(old)), toml::Value::Table(new)) => deep_merge(old, new),
            _ => {
                into.insert(key.clone(), value.clone());
            }
        }
    }
}

fn apply_env(into: &mut toml::Table, section: &str, env: &dyn Env) -> Result<(), ConfigError> {
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
    for (key, kind) in LEAVES {
        let name = format!("AUTUMN_{name}__{}", key.to_ascii_uppercase());
        let Ok(raw) = env.var(&name) else {
            continue;
        };
        let bad = || ConfigError(format!("{name}: can not read {raw:?}"));
        let value = match kind {
            Kind::Text => toml::Value::String(raw.clone()),
            Kind::Unsigned | Kind::Signed => {
                let value: i64 = raw.trim().parse().map_err(|_| bad())?;
                if value < 0 && matches!(kind, Kind::Unsigned) {
                    return Err(bad());
                }
                toml::Value::Integer(value)
            }
            Kind::Bool => match raw.trim() {
                "true" | "1" => toml::Value::Boolean(true),
                "false" | "0" => toml::Value::Boolean(false),
                _ => return Err(bad()),
            },
            Kind::List => toml::Value::Array(
                raw.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| toml::Value::String(item.to_owned()))
                    .collect(),
            ),
        };
        into.insert((*key).to_owned(), value);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
