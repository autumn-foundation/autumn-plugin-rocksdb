# Changelog

## 0.1.0

- `RocksDbPlugin` and the `RocksDb` extractor.
- `RocksDb` and `Keyspace`: `get`, `get_json`, `exists`, `put`, `put_json` and `delete`.
- `Scan`: prefix and range scans in pages, with a cursor.
- `Batch`: atomic writes over column families.
- `with_db` for the full `rocksdb` API. `checkpoint` for backups.
- `RocksCache`: the Autumn app cache, with a TTL.
- `RocksSessionStore`: the Autumn session store, with a TTL.
- A compaction filter that removes expired cache entries and sessions.
- A timeout, a call limit, size limits and scan page limits.
- A readiness check, Prometheus metrics and a flush at shutdown.
