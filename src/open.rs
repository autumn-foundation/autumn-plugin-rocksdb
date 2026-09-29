//! RocksDB options and the open steps.
//!
//! # Contract
//!
//! - `:memory:` opens a database in a new in-memory file system. Two opens never share data.
//! - The plugin opens each existing column family, each configured one and the reserved ones in use.
//!   A writable open makes the missing ones.
//! - A read-only open refuses a configured column family that does not exist.
//! - `autumn_cache` and `autumn_sessions` have a compaction filter. It removes expired envelopes.
//! - Setup hooks run in order after the open. A failed hook stops the open.

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

/// The directory of an in-memory database in its in-memory file system.
const MEMORY_PATH: &str = "/autumn-plugin-rocksdb";

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
        MEMORY_PATH
    } else {
        config.path.as_str()
    };
    let names = column_family_names(config, &options, path)?;
    let descriptors = names.iter().map(|name| {
        let cf_options = if name == CACHE_CF || name == SESSIONS_CF {
            ttl_options(&options)
        } else {
            options.clone()
        };
        ColumnFamilyDescriptor::new(name, cf_options)
    });
    let db = if config.is_read_only() {
        Database::open_cf_descriptors_read_only(&options, path, descriptors, false)?
    } else {
        Database::open_cf_descriptors(&options, path, descriptors)?
    };
    for setup in setups {
        setup(&db)?;
    }
    Ok(Opened {
        db,
        column_families: names,
    })
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

/// Gives the column families to open: `default`, the existing ones, the configured ones and the reserved ones in use.
fn column_family_names(
    config: &RocksDbConfig,
    options: &Options,
    path: &str,
) -> Result<Vec<String>, RocksDbError> {
    // A missing database has no column families. `list_cf` then fails.
    let existing = Database::list_cf(options, path).unwrap_or_default();
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
