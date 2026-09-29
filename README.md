# autumn-plugin-rocksdb

An [Autumn](https://github.com/autumn-foundation/autumn) plugin for [RocksDB](https://rocksdb.org). Handlers read and write keys in an embedded store. The same database can hold the app cache and the sessions.

- Key-value API: `get`, `put`, `delete`, JSON values, column families and atomic batches.
- Scans: prefix and range scans in pages, with a cursor for the next page.
- Cache: `RocksCache` is the Autumn app cache. Each entry expires. Entries survive a restart.
- Sessions: `RocksSessionStore` is the Autumn session store. Sessions survive a restart.
- Limits: a timeout, a call limit, and size limits for keys, values, batches and scan pages.
- Operations: a readiness check, Prometheus metrics, checkpoints, and a flush and close at shutdown.
- Tests: use an in-memory database. The tests need no server.

## Install

```toml
[dependencies]
autumn-plugin-rocksdb = "0.1"
```

```rust,ignore
use autumn_plugin_rocksdb::{RocksDb, RocksDbPlugin, RocksDbResultExt as _};
use autumn_web::prelude::*;

#[derive(serde::Deserialize, serde::Serialize)]
struct Profile {
    name: String,
}

#[get("/profiles/{id}")]
async fn profile(db: RocksDb, Path(id): Path<u64>) -> AutumnResult<Json<Profile>> {
    let profile = db
        .cf("profiles")
        .get_json::<Profile>(id.to_be_bytes())
        .await
        .or_http()?;
    profile.map(Json).ok_or_else(|| AutumnError::not_found_msg("no profile"))
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(RocksDbPlugin::new())
        .routes(routes![profile])
        .run()
        .await;
}
```

Add `profiles` to `column_families` in `autumn.toml`. See [Configuration](#configuration).

The first build compiles RocksDB from C++ source. It takes 10 to 15 minutes on 4 CPU cores. It needs a C++ compiler and `clang`.

The `lz4`, `zstd` and `snappy` features are on by default. They build these compression libraries into RocksDB. RocksDB writes Snappy blocks by default, so `snappy` lets the plugin read databases from other tools. The `zlib` feature adds zlib.

An app can have one RocksDB plugin only. Use column families to keep data sets apart.

The plugin re-exports the `rocksdb` crate and uses its types in `with_db` and setup hooks. A `rocksdb` update is a breaking release of this crate.

## Configuration

The plugin reads `[rocksdb]` in `autumn.toml`. Profile sections and profile files override it. `AUTUMN_ROCKSDB__<KEY>` variables override all files. A list variable has comma-separated items.

```toml
[rocksdb]
path = "data/rocksdb"          # ":memory:" gives an in-memory database
access_mode = "read_write"     # or "read_only"
create_if_missing = true
column_families = ["profiles", "orders"]
timeout_ms = 5000              # includes the wait for a call slot
max_concurrent_calls = 64
max_key_bytes = 16384
max_value_bytes = 16777216
max_batch_bytes = 67108864
max_scan_entries = 1000
max_scan_bytes = 16777216
sync_writes = false            # true waits for the write-ahead log on disk
cache = false                  # true installs RocksCache as the app cache
cache_ttl_secs = 86400         # the longest cache TTL
sessions = false               # true installs RocksSessionStore
# session_ttl_secs = 86400     # default: session.max_age_secs of Autumn
health_check = true
flush_on_shutdown = true
compression = "lz4"            # "none", "lz4", "zstd" or "snappy"
bloom_filter_bits = 10         # 0 turns the bloom filter off
# block_cache_bytes = 67108864
# write_buffer_bytes = 67108864
# max_open_files = -1
# max_background_jobs = 4
```

The default path is `:memory:`. Set `path` to keep data after a restart.

On Unix, the plugin makes a new database directory with mode `0700`. Only the owner can read it.

Use `RocksDbPlugin::config` to give a configuration in code. Use `RocksDbPlugin::configure` to change the configuration after the plugin reads it.

## Reads and writes

The methods of `RocksDb` use the `default` column family. `db.cf("name")` gives a `Keyspace` for another column family. Add each column family to `column_families`.

```rust,ignore
db.put("user:1", "Ada").await?;
let name = db.get("user:1").await?;           // Option<Vec<u8>>
db.cf("profiles").put_json("user:1", &profile).await?;
db.delete("user:1").await?;
```

A batch writes all entries or none:

```rust,ignore
db.batch()
    .put("a", "1")
    .put_in("profiles", "b", "2")
    .delete("old")
    .commit()
    .await?;
```

A write that times out after it starts can still complete. Its outcome is unknown. A write that times out before it starts never runs.

## Scans

A scan reads one page in key order. `Page::next` is the cursor of the next page. It is `None` when no entry is left.

```rust,ignore
let mut cursor = None;
loop {
    let mut scan = db.scan().prefix("user:").limit(100);
    if let Some(after) = cursor.take() {
        scan = scan.after(after);
    }
    let page = scan.fetch().await?;
    for entry in &page.entries {
        let profile: Profile = entry.value_as()?;
    }
    match page.next {
        Some(next) => cursor = Some(next),
        None => break,
    }
}
```

A page also ends at `max_scan_bytes`. One entry above that limit gives `EntryTooLarge` with the key of the entry. Read it with `get`, or give the key to `after` to skip it.

## Cache and sessions

Set `cache = true`. The plugin installs `RocksCache` as the app cache. Autumn keeps one app cache for each process. `#[cached]` functions then store their values in RocksDB.

- Each cache entry expires. `cache_ttl_secs` is the longest TTL. An entry without a TTL gets it.
- A cache call does not wait. If all call slots are busy, a read is a miss and a write does nothing.
- A cache write does not wait for a RocksDB write stall. A stalled write does nothing.
- The cache has no size limit. Expired entries leave the disk at compaction.

Set `sessions = true`. The plugin installs `RocksSessionStore`. Each session expires `session_ttl_secs` after its last save. The key on disk is the SHA-256 hash of the session ID, not the ID.

A compaction filter removes expired cache entries and sessions from disk. A read after the expiry is a miss before the filter runs.

The cache and the sessions use the reserved column families `autumn_cache` and `autumn_sessions`. The user API refuses names with the `autumn_` prefix.

## Setup hooks and the full API

A setup hook runs at startup, before the first request. A failed hook stops the boot.

```rust,ignore
RocksDbPlugin::new().setup(|db| db.put(b"schema_version", b"1"))
```

`with_db` gives the full `rocksdb` API on a blocking thread, for example for a snapshot. The timeout and the call limit apply. `sync_writes` does not apply to writes in `with_db` or in setup hooks.

```rust,ignore
let value = db.with_db(|db| db.snapshot().get(b"key")).await?;
```

`checkpoint` makes a consistent copy of a file database, for example for a backup. The directory must not exist. On Unix, the copy has mode `0700`. The copy holds all data, so keep it safe.

## Errors

`RocksDbError::status` gives the HTTP status. `or_http()` converts a result for a handler.

| Error | Status |
|-------|--------|
| `Timeout` | 504 |
| `ShuttingDown`, and the kinds `Busy`, `TryAgain`, `TimedOut` and `ShutdownInProgress` | 503 |
| `KeyTooLarge`, `ValueTooLarge`, `BatchTooLarge` | 413 |
| `ScanLimit` | 400 |
| All others | 500 |

The error text and the `Debug` output never show a RocksDB message, because it can hold a file path. `detail()` gives the full message. Do not show it to users. `kind()` gives the `ErrorKind`.

## Operations

- Readiness: `/actuator/health` has a `rocksdb` component. It is down before startup and after the close. It is also down when writes stop, or when the background error count grew in the last 60 seconds.
- Metrics: `rocksdb_calls_started_total`, `rocksdb_calls_total`, `rocksdb_calls_open`, `rocksdb_read_bytes_total`, `rocksdb_written_bytes_total`, `rocksdb_cache_requests_total` and `rocksdb_background_errors`. Gauges for each column family give keys, file sizes, memtables and pending compaction.
- Shutdown: Autumn marks the shutdown before it drains the requests. At the mark, the plugin flushes the write-ahead log and the memtables. The database stays open for the drain.
- Close: after the drain, the plugin refuses new calls. It waits up to 5 seconds for open calls. Then it flushes a writable file database, if `flush_on_shutdown` is `true`. Then it closes the database.
- One process can open a database for writes. Other processes can open it with `access_mode = "read_only"`. A read-only open is a view of the database at the time of the open. It does not see later writes.

## License

Apache-2.0.
