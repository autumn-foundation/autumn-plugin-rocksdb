# Planning

This document records the plan for `autumn-plugin-rocksdb`. It uses three methods: brainstorming, reverse brainstorming and six thinking hats. The last sections give the decisions, the architecture and the TDD plan.

## Goal

An Autumn app keeps key-value data in RocksDB. The app does one install step. The plugin sets limits on call time, memory and blocking threads. The same database can also hold the app cache and the sessions.

## 1. Brainstorming

Write all ideas first. Do not judge them in this step.

1. A handler extractor `RocksDb` that gives a database handle.
2. Byte operations: `get`, `put`, `delete` and `exists`.
3. Typed operations: `get_json` and `put_json` with `serde`.
4. Column families as named keyspaces: `db.cf("users").get(key)`.
5. An atomic write batch over many keys and column families.
6. A prefix scan and a range scan, in pages, with a cursor.
7. A page limit and a byte limit for each scan.
8. A key size limit and a value size limit for each write.
9. A timeout for each call.
10. A limit on the calls that run at the same time.
11. Each call runs on a blocking thread.
12. Read `[rocksdb]` in `autumn.toml`, with profiles and `AUTUMN_ROCKSDB__*` variables.
13. A file database, an in-memory database and a read-only mode.
14. RocksDB tuning: block cache, write buffer, open files, background jobs, compression and bloom filters.
15. A setup hook: Rust code that runs on the database before requests.
16. `with_db`: run Rust code with the full `rocksdb` API, for example a snapshot.
17. A checkpoint: a consistent copy of the database for backups.
18. An Autumn `Cache` backend that keeps cache entries on disk across restarts.
19. An Autumn `SessionStore` backend that keeps sessions across restarts.
20. A time to live (TTL) for cache entries and sessions. A compaction filter removes expired entries.
21. A readiness check from RocksDB background errors and write stops.
22. Prometheus metrics: calls by outcome, open calls, bytes, keys and file sizes.
23. A flush of the memtables and the write-ahead log (WAL) at shutdown.
24. Map RocksDB error kinds to HTTP status codes.
25. A TTL for keys in user column families.
26. Transactions and compare-and-swap.
27. Merge operators, for example counters.
28. A secondary instance that follows a primary.
29. Encryption at rest.
30. Replication to other nodes.
31. An admin endpoint for statistics and compaction.
32. More than one database in one app.

## 2. Reverse brainstorming

Question: "How can we make this plugin fail?" Each answer gives a countermeasure.

| How to make it fail | Countermeasure |
|---------------------|----------------|
| Block the async runtime with disk I/O. | Run each call on a blocking thread. |
| Start too many blocking threads. | A semaphore limits the open calls. The permit stays with the call until the thread ends. |
| Wait without end for a slow disk. | Each call has a deadline. The wait for a permit counts. |
| Read a very large scan into memory. | A page limit and a byte limit. A scan gives a cursor for the next page. |
| Write a very large key or value. | A key size limit and a value size limit. The plugin refuses the write before the call. |
| Open a database that has column families that the config does not name. RocksDB refuses the open. | List the column families that exist. Open all of them. |
| Use a column family that does not exist. | Give a typed error. Do not panic. |
| Change the cache or session data through the user API. | Names that start with `autumn_` are reserved. The user API refuses them. |
| Open the same path in two processes. | RocksDB holds a lock file. The second open fails and stops the boot with a clear error. |
| Lose data at shutdown. | Refuse new calls. Wait for open calls. Flush the WAL and the memtables. Close the database. |
| Autumn ends the process before the shutdown hooks. | A watch task starts the shutdown when Autumn marks it. |
| Keep expired sessions and cache entries for ever. | Check the expiry on each read. A compaction filter removes expired entries. |
| Log secrets. Keys can hold session IDs or email addresses. | Never log keys or values. |
| Send file paths to HTTP clients. A RocksDB message can hold a path. | The error text gives the error kind only. `detail()` gives the full message. |
| Panic in a request path. | No `unwrap`, `expect` or `panic` in library code. Clippy denies them. |
| Start with a bad configuration and fail later. | Validate in `build`. Open the database in the startup hook. A failure stops the boot. |
| Call the sync `Cache` trait on an async worker thread. | Use `block_in_place` on a multi-thread runtime. |
| Use the session store before startup. | Give a `SessionStoreError`. Do not panic. |
| Make tests slow or flaky. | Tests use an in-memory database or a temporary directory. No test uses the network. |

## 3. Six thinking hats

### White hat (facts)

- Autumn 0.8 gives `Plugin`, `contract`, `on_startup`, `on_shutdown`, `health_indicator`, `metrics_source`, `config_section` and `with_session_store`.
- `AppState::set_cache` installs a global cache during startup. The `Cache` trait is synchronous.
- The `SessionStore` trait is async. It gets no TTL. The Autumn session config has `max_age_secs`.
- The `rocksdb` crate 0.25 wraps RocksDB 11. It builds RocksDB from C++ source. The MSRV is 1.88.
- `DB` is `Send` and `Sync`. Reads and writes need `&DB` only.
- RocksDB calls block. Nothing can stop a RocksDB call before it returns.
- `Env::mem_env` gives an in-memory file system.
- `DB::list_cf` gives the column families of a database that exists.
- `delete_range_cf` removes a key range in one call.
- A compaction filter sees each key and value during compaction. It can remove the entry.
- The properties `rocksdb.background-errors` and `rocksdb.is-write-stopped` show the database health.
- A `rocksdb::Error` has a kind. Its message can hold a file path.
- Valid UTF-8 text never has the byte `0xFF`.

### Red hat (feelings)

- Users want one line of setup and one line for each read or write.
- A lost session or a lost write at shutdown is the worst result.
- A scan that stops without a signal is not safe. A cursor shows where the page ends.

### Black hat (risks)

- The C++ build is slow. CI must cache it.
- A timed-out call keeps its blocking thread until RocksDB returns.
- A system clock that jumps changes the expiry of entries.
- An in-memory database loses all data at shutdown. This is correct. Tell users about it.
- An unbounded cache fills the disk. The compaction filter removes expired entries only.

### Yellow hat (benefits)

- An embedded store needs no server. Tests need no fake and no network.
- One process gets a cache and sessions that survive a restart.
- Column families keep data sets apart and give each one its own tuning.

### Green hat (new ideas)

- A value envelope for cache and session entries: a version byte, an expiry time and the payload.
- Cache and session keys are UTF-8. The range `[b"", [0xFF])` covers all of them. `clear` is one range delete.
- A setup hook runs before the first request. It can seed data.
- `with_db` gives the full `rocksdb` API under the same timeout and call limit.

### Blue hat (process)

- Pure modules first. Each gets a `# Contract` section and tests.
- Glue modules next. Each gets tests on an in-memory or temporary database.
- Each cycle is red, then green, then refactor. Each phase gets a commit.
- At the end, review agents check the code from different angles.

## 4. Decisions

### In scope

Ideas 1 to 24.

### Out of scope

| Idea | Reason |
|------|--------|
| 25. TTL in user column families | User values stay raw bytes. Other RocksDB tools can read them. Use the cache for TTL data. |
| 26. Transactions | They need `TransactionDB`, a different database type. Use a write batch or `with_db`. |
| 27. Merge operators | A merge operator must be the same at each open. Add later if users ask. |
| 28. Secondary instances | Few web apps need them. Add later if users ask. |
| 29. Encryption at rest | The `rocksdb` crate does not expose it. Use disk encryption. |
| 30. Replication | RocksDB is a local store. Use a server database for many nodes. |
| 31. Admin endpoint | It is a security risk. `with_db` gives compaction and properties. |
| 32. More than one database | The plugin name is fixed. Use column families. |

## 5. Architecture

| Module | Kind | Job |
|--------|------|-----|
| `config` | pure | `RocksDbConfig`, layering and validation. |
| `envelope` | pure | The TTL value envelope and the expiry rule. |
| `bounds` | pure | Prefix ends and scan bounds. |
| `error` | data | `RocksDbError`, `ErrorKind` and the HTTP status map. |
| `metrics` | data | Counters, property gauges and metric families. |
| `open` | glue | RocksDB options and the open steps. |
| `client` | glue | `RocksDb`, `Keyspace`, `Batch` and `Scan`: calls with a deadline and limits. |
| `cache` | glue | `RocksCache`: the Autumn `Cache` backend. |
| `session` | glue | `RocksSessionStore`: the Autumn `SessionStore` backend. |
| `plugin` | glue | `RocksDbPlugin`, the extractor and the metrics source. |
| `health` | glue | The readiness check. |

## 6. TDD plan

Each item is one cycle. Red: write a test that fails. Green: write the minimum code. Refactor: clean up with all tests green.

1. `envelope`: encode and decode. No expiry. Past and future expiry. Bad bytes. Property: decode inverts encode.
2. `bounds`: the prefix end of plain, `0xFF` and empty prefixes. Scan bounds with a cursor. Property: each key with the prefix is in the bounds.
3. `error`: the kind map, the text, the detail and the HTTP status map.
4. `config`: defaults, TOML, profile layers, environment variables and validation.
5. `open`: file, in-memory and read-only opens. Existing column families. Setup hooks.
6. `client`: get, put, delete, JSON, batch, scan pages, limits, reserved names, timeout, call limit and shutdown.
7. `cache`: the `Cache` trait, TTL, clear, restart survival and the compaction filter.
8. `session`: load, save, destroy, TTL and use before startup.
9. `metrics` and `health`: counters, properties and states.
10. `plugin`: the extractor, the cache, the sessions and boot errors, in `TestApp`.

## 7. Review

Five review agents read the code after the build. Each agent had one angle. Each finding got a red test first, then a fix.

| Angle | Main findings | Fixes |
|-------|---------------|-------|
| Correctness and concurrency | Autumn marks the shutdown before the drain, and the watch closed the database at the mark. A timed-out call that waited in the blocking queue still ran. The cache skipped the call slots. | The mark only flushes. The shutdown hook closes. A queued call that passed its deadline does no work. The cache takes a slot without a wait. |
| Security and privacy | Session IDs were plain keys on disk. Directories were readable by other users. `Debug` output showed paths. A near-miss of `:memory:` made a real database. Cache entries without a TTL never expired. Batches had no size limit. | Session keys are SHA-256 hashes. New directories have mode `0700`. `Debug` output is the safe text. The path check refuses near misses. New keys `cache_ttl_secs` and `max_batch_bytes`. Expiry fails closed on a clock before 1970. |
| RocksDB semantics | Snappy files from other tools were not readable. The background error count never goes down, so the check stayed down. A read-only open hid the real open error. The in-memory path was at the file system root. | `snappy` is a default feature. The check uses errors of the last 60 seconds. The read-only open gives the real error. The in-memory path is in the temp directory. |
| API and documentation | The public error exposed a `rocksdb` enum. There was no public close. `ScanLimit` gave 500. Some docs did not match the code. | A new `ErrorKind`. Public `close` and `flush`. `ScanLimit` gives 400. `EntryTooLarge` has the key. The docs match the code. |
| Test quality | Some contract bullets had no test. Some tests used short sleeps. | New tests kill each reported mutant. TTL tests use stored envelopes, not sleeps. |

These items stay out of scope for 0.1:

| Item | Reason |
|------|--------|
| A scan limit for deleted keys (`max_skippable_internal_keys`) | A large delete would then give an error. The call slot limit bounds the cost. |
| A separate slot pool for sessions | The call limit covers all callers. Raise `max_concurrent_calls` for a busy app. |
| A call slot for the readiness check | The check reads in-memory properties and waits 1 second at most. |
| A size limit for the cache | Each entry expires. The compaction filter removes it from disk. |
| `put_json` in a batch, and a `Keyspace` argument for `put_in` | Encode the value first. Use the column family name. |
