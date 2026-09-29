//! RocksDB options and the open steps.
//!
//! # Contract
//!
//! - `:memory:` opens a database in a new in-memory file system. Two opens never share data.
//! - The plugin opens each column family that exists, each configured one and the reserved ones in use.
//!   A writable open makes the missing ones.
//! - A read-only open refuses a configured column family that does not exist.
//! - `autumn_cache` and `autumn_sessions` have a compaction filter. It removes expired envelopes.
//! - Setup hooks run in order after the open. A failed hook stops the open.
//! - On Unix, a new database directory and its new parents have mode `0700`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rocksdb::compaction_filter::Decision;
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType, DBWithThreadMode, Env,
    Options, SingleThreaded,
};

use crate::config::{Compression, RocksDbConfig};
use crate::envelope;
use crate::error::RocksDbError;

/// The RocksDB database type of the plugin.
///
/// The type does not change when a crate turns on the `multi-threaded-cf` feature of `rocksdb`.
pub type Database = DBWithThreadMode<SingleThreaded>;

/// Rust code that runs on the database at startup.
pub type Setup = Arc<dyn Fn(&Database) -> Result<(), rocksdb::Error> + Send + Sync>;

/// The column family of [`RocksCache`](crate::RocksCache).
pub const CACHE_CF: &str = "autumn_cache";

/// The column family of [`RocksSessionStore`](crate::RocksSessionStore).
pub const SESSIONS_CF: &str = "autumn_sessions";

/// The largest info log file of an in-memory database.
const MEMORY_LOG_BYTES: usize = 1024 * 1024;

/// The directory name of an in-memory database.
const MEMORY_DIR: &str = "autumn-plugin-rocksdb-memory";

/// The directory of an in-memory database.
///
/// `rocksdb` makes this directory on the real file system before each open. It stays empty.
/// The temp directory is writable for a user that is not root.
pub(crate) fn memory_path() -> PathBuf {
    std::env::temp_dir().join(MEMORY_DIR)
}

/// An open database.
pub(crate) struct Opened {
    pub(crate) db: Database,
    /// The names of all open column families, `default` first.
    pub(crate) column_families: Vec<String>,
}

/// Opens the database and runs the setup hooks.
pub(crate) fn open(config: &RocksDbConfig, setups: &[Setup]) -> Result<Opened, RocksDbError> {
    config.validate()?;
    let mut options = options(config);
    let path = if config.is_in_memory() {
        options.set_env(&Env::mem_env()?);
        // The info log is in memory too. Keep it small.
        options.set_max_log_file_size(MEMORY_LOG_BYTES);
        options.set_keep_log_file_num(2);
        memory_path()
    } else {
        let path = PathBuf::from(&config.path);
        if !config.is_read_only() && config.create_if_missing && !path.exists() {
            create_private_dir(&path).map_err(|err| RocksDbError::Database {
                kind: crate::error::ErrorKind::Io,
                detail: err.to_string(),
            })?;
        }
        path
    };
    let names = column_family_names(config, &options, &path)?;
    let descriptors = names.iter().map(|name| {
        let cf_options = if name == CACHE_CF || name == SESSIONS_CF {
            ttl_options(&options)
        } else {
            options.clone()
        };
        ColumnFamilyDescriptor::new(name, cf_options)
    });
    let db = if config.is_read_only() {
        Database::open_cf_descriptors_read_only(&options, &path, descriptors, false)?
    } else {
        Database::open_cf_descriptors(&options, &path, descriptors)?
    };
    for setup in setups {
        setup(&db)?;
    }
    Ok(Opened {
        db,
        column_families: names,
    })
}

/// Makes `path` and its missing parents. On Unix, only the owner can open the new directories.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// On Unix, lets only the owner open `dir`.
pub(crate) fn make_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Gives the database options of `config`.
fn options(config: &RocksDbConfig) -> Options {
    let mut options = Options::default();
    let writable = !config.is_read_only();
    options.create_if_missing(writable && config.create_if_missing);
    options.create_missing_column_families(writable);
    if let Some(files) = config.max_open_files {
        options.set_max_open_files(files);
    }
    if let Some(jobs) = config.max_background_jobs {
        options.set_max_background_jobs(jobs);
    }
    if let Some(bytes) = config.write_buffer_bytes {
        options.set_write_buffer_size(bytes);
    }
    options.set_compression_type(match config.compression {
        Compression::Lz4 => DBCompressionType::Lz4,
        Compression::Zstd => DBCompressionType::Zstd,
        Compression::Snappy => DBCompressionType::Snappy,
        Compression::None => DBCompressionType::None,
    });
    let mut table = BlockBasedOptions::default();
    if let Some(bytes) = config.block_cache_bytes {
        table.set_block_cache(&Cache::new_lru_cache(bytes));
    }
    if config.bloom_filter_bits > 0 {
        table.set_bloom_filter(f64::from(config.bloom_filter_bits), false);
    }
    options.set_block_based_table_factory(&table);
    options
}

/// Adds the compaction filter that removes expired envelopes.
fn ttl_options(options: &Options) -> Options {
    let mut options = options.clone();
    options.set_compaction_filter("autumn_ttl", |_level: u32, _key: &[u8], value: &[u8]| {
        if envelope::keep(value, envelope::now_ms()) {
            Decision::Keep
        } else {
            Decision::Remove
        }
    });
    options
}

/// Gives the column families to open: `default`, the ones that exist, the configured ones and the reserved ones in use.
fn column_family_names(
    config: &RocksDbConfig,
    options: &Options,
    path: &Path,
) -> Result<Vec<String>, RocksDbError> {
    // A missing database has no column families. `list_cf` then fails.
    // A read-only open needs a database, so the error is the real cause.
    let existing = match Database::list_cf(options, path) {
        Ok(existing) => existing,
        Err(err) if config.is_read_only() => return Err(err.into()),
        Err(_) => Vec::new(),
    };
    let mut names = vec![rocksdb::DEFAULT_COLUMN_FAMILY_NAME.to_owned()];
    if config.is_read_only()
        && let Some(name) = config
            .column_families
            .iter()
            .find(|n| !existing.contains(n))
    {
        return Err(RocksDbError::UnknownColumnFamily { name: name.clone() });
    }
    let reserved = [(config.cache, CACHE_CF), (config.sessions, SESSIONS_CF)]
        .into_iter()
        .filter(|(used, _)| *used)
        .map(|(_, name)| name.to_owned());
    for name in existing
        .into_iter()
        .chain(config.column_families.iter().cloned())
        .chain(reserved)
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests;
