# Changelog

## Unreleased

- Autumn 0.8: the `autumn-web` range is now `>=0.8, <0.9`.
- `RocksDbPlugin` declares a `Plugin::contract` for `autumn plugin-check`.

## 0.1.0 (2026-09-29)

- `RocksDbPlugin`, the `RocksDb` extractor and `RocksDb::from_state`.
- `RocksDbConfig`: the `[rocksdb]` section, profiles, `AUTUMN_ROCKSDB__*` variables and validation. `config_section`, `config` and `configure`.
- `RocksDb` and `Keyspace`: `get`, `get_json`, `exists`, `put`, `put_json` and `delete`.
- `Scan`: prefix and range scans in pages, with a cursor.
- `Batch`: atomic writes over column families.
- `with_db` for the full `rocksdb` API. `checkpoint` for backups. `flush` and `close`.
- Setup hooks that run before the first request.
- `RocksCache`: the Autumn app cache. Each entry expires.
- `RocksSessionStore`: the Autumn session store. The key on disk is the SHA-256 hash of the session ID.
- A compaction filter that removes expired cache entries and sessions.
- `RocksDbError`, `ErrorKind`, `RocksDbResultExt::or_http` and the HTTP status map.
- A timeout, a call limit, size limits for keys, values and batches, and scan page limits.
- A readiness check, Prometheus metrics, a flush at the shutdown mark and a close after the drain.
