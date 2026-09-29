# CLAUDE.md

Guidance for agents that work on this crate.

## What this crate is

`autumn-plugin-rocksdb` is an Autumn plugin. Autumn is `autumn-web` 0.7. Handlers use RocksDB through the `RocksDb` extractor. The plugin can also be the app cache and the session store. Read `docs/planning.md` before a design change.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo check --locked --lib --no-default-features
cargo test --locked --all-targets --all-features
cargo +1.88.0 check --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS=-D\ warnings cargo doc --locked --no-deps --all-features
cargo llvm-cov --locked --all-features --ignore-filename-regex '/tests\.rs$' --fail-under-lines 90
```

The MSRV is 1.88. The first build compiles RocksDB from C++ source. It takes some minutes. Each new feature set or toolchain builds it again.

## Architecture

| Module | Kind | Job |
|--------|------|-----|
| `config` | pure | `RocksDbConfig`, layering and validation. |
| `envelope` | pure | The TTL value envelope and the expiry rule. |
| `bounds` | pure | Prefix ends and scan bounds. |
| `error` | data | `RocksDbError` and the HTTP status map. |
| `metrics` | data | Counters, property gauges and metric families. |
| `open` | glue | RocksDB options, column families, the compaction filter and setup hooks. |
| `client` | glue | `RocksDb`, `Keyspace`, `Scan` and `Batch`: calls with a deadline and limits. |
| `cache` | glue | `RocksCache`: the Autumn `Cache` backend. |
| `session` | glue | `RocksSessionStore`: the Autumn `SessionStore` backend. |
| `plugin` | glue | `RocksDbPlugin`, the extractor and the metrics source. |
| `health` | glue | The readiness check. |

Each module has a `# Contract` doc section. Change the contract first. Then change the tests. Then change the code.

## Rules

- Work red, green, refactor. Write the failing test first.
- Put unit tests in `src/<module>/tests.rs`. The coverage gate ignores these files.
- Production code has no `unwrap`, `expect` or `panic`. Clippy denies them outside tests.
- Use `open::Database`, not `rocksdb::DB`. Another crate can change `rocksdb::DB` with the `multi-threaded-cf` feature.
- Each RocksDB call from async code runs on a blocking thread. The sync `Cache` trait uses `block_in_place` on a multi-thread runtime.
- Each user call has a deadline and a call slot. The slot stays taken until RocksDB returns.
- Never log keys, values, session IDs or RocksDB messages. A RocksDB message can hold a file path.
- Column families with the `autumn_` prefix belong to the plugin. The user API refuses them.
- Metric names start with `rocksdb_`. They must not start with `autumn_`.
- No test uses the network. Tests use `:memory:` or a temporary directory.

## Test notes

- `TestApp` runs startup hooks but not shutdown hooks. `plugin::tests` tests the shutdown watch.
- `TestApp` uses its own memory session store for HTTP requests. The integration test uses the store in the app state.
- `AppState::set_cache` sets a process-wide cache. Tests that turn on `cache` must not depend on other tests.
- A slow call for timeout tests: `db.with_db(|_| { std::thread::sleep(..); Ok(()) })`.

## Documentation style

Write docs and comments in ASD-STE100 style: short sentences, active voice, simple present tense, one instruction per sentence. Keep instructions at 20 words or fewer and descriptions at 25 words or fewer.
