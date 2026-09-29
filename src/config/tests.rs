#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::path::Path;

use autumn_web::config::MockEnv;

use super::*;

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn env_for(dir: &Path) -> MockEnv {
    MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.to_str().unwrap())
}

fn resolve(env: &MockEnv) -> Result<RocksDbConfig, ConfigError> {
    RocksDbConfig::resolve_with_env("rocksdb", env)
}

fn invalid(change: impl FnOnce(&mut RocksDbConfig)) -> String {
    let mut config = RocksDbConfig::default();
    change(&mut config);
    config.validate().unwrap_err().to_string()
}

#[test]
fn defaults_are_safe() {
    let config = RocksDbConfig::default();
    assert_eq!(config.path, IN_MEMORY);
    assert!(config.is_in_memory());
    assert!(!config.is_read_only());
    assert!(config.create_if_missing);
    assert!(config.column_families.is_empty());
    assert_eq!(config.timeout(), Duration::from_secs(5));
    assert_eq!(config.max_concurrent_calls, 64);
    assert_eq!(config.max_key_bytes, 16 * 1024);
    assert_eq!(config.max_value_bytes, 16 * 1024 * 1024);
    assert_eq!(config.max_scan_entries, 1_000);
    assert_eq!(config.max_scan_bytes, 16 * 1024 * 1024);
    assert!(!config.sync_writes);
    assert!(!config.cache);
    assert!(!config.sessions);
    assert_eq!(config.session_ttl_secs, None);
    assert_eq!(config.cache_ttl_secs, 86_400);
    assert!(config.health_check);
    assert!(config.flush_on_shutdown);
    assert_eq!(config.bloom_filter_bits, 10);
    assert!(config.compression.is_available());
    config.validate().unwrap();
}

#[test]
fn no_file_gives_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve(&env_for(dir.path())).unwrap(),
        RocksDbConfig::default()
    );
}

#[test]
fn reads_the_section_from_autumn_toml() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "autumn.toml",
        r#"
[rocksdb]
path = "data/app"
access_mode = "read_only"
create_if_missing = false
column_families = ["users", "orders"]
timeout_ms = 250
max_concurrent_calls = 3
max_key_bytes = 100
max_value_bytes = 1000
max_scan_entries = 10
max_scan_bytes = 5000
sync_writes = true
session_ttl_secs = 60
cache_ttl_secs = 120
health_check = false
flush_on_shutdown = false
block_cache_bytes = 1048576
write_buffer_bytes = 2097152
max_open_files = -1
max_background_jobs = 2
compression = "none"
bloom_filter_bits = 0
"#,
    );
    let config = resolve(&env_for(dir.path())).unwrap();
    assert_eq!(config.path, "data/app");
    assert!(config.is_read_only());
    assert!(!config.create_if_missing);
    assert_eq!(config.column_families, ["users", "orders"]);
    assert_eq!(config.timeout(), Duration::from_millis(250));
    assert_eq!(config.max_concurrent_calls, 3);
    assert_eq!(config.max_key_bytes, 100);
    assert_eq!(config.max_value_bytes, 1000);
    assert_eq!(config.max_scan_entries, 10);
    assert_eq!(config.max_scan_bytes, 5000);
    assert!(config.sync_writes);
    assert_eq!(config.session_ttl_secs, Some(60));
    assert_eq!(config.cache_ttl_secs, 120);
    assert!(!config.health_check);
    assert!(!config.flush_on_shutdown);
    assert_eq!(config.block_cache_bytes, Some(1_048_576));
    assert_eq!(config.write_buffer_bytes, Some(2_097_152));
    assert_eq!(config.max_open_files, Some(-1));
    assert_eq!(config.max_background_jobs, Some(2));
    assert_eq!(config.compression, Compression::None);
    assert_eq!(config.bloom_filter_bits, 0);
}

#[test]
fn reads_a_custom_section() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "autumn.toml", "[store]\ntimeout_ms = 7\n");
    let config = RocksDbConfig::resolve_with_env("store", &env_for(dir.path())).unwrap();
    assert_eq!(config.timeout_ms, 7);
}

#[test]
fn inline_profile_overrides_the_base() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "autumn.toml",
        "[rocksdb]\npath = \"dev\"\ntimeout_ms = 5\n[profile.prod.rocksdb]\npath = \"prod\"\n",
    );
    let config = resolve(&env_for(dir.path()).with("AUTUMN_ENV", "production")).unwrap();
    assert_eq!(config.path, "prod");
    assert_eq!(config.timeout_ms, 5);
    let config = resolve(&env_for(dir.path())).unwrap();
    assert_eq!(config.path, "dev");
}

#[test]
fn a_profile_file_overrides_the_inline_profile() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "autumn.toml",
        "[rocksdb]\npath = \"base\"\n[profile.dev.rocksdb]\npath = \"inline\"\ntimeout_ms = 9\n",
    );
    write(
        dir.path(),
        "autumn-dev.toml",
        "[rocksdb]\npath = \"file\"\n",
    );
    let config = resolve(&env_for(dir.path())).unwrap();
    assert_eq!(config.path, "file");
    assert_eq!(config.timeout_ms, 9);
}

#[test]
fn a_release_build_selects_prod() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "autumn-prod.toml",
        "[rocksdb]\npath = \"prod\"\n",
    );
    let config = resolve(&env_for(dir.path()).with("AUTUMN_IS_DEBUG", "0")).unwrap();
    assert_eq!(config.path, "prod");
}

#[test]
fn variables_override_all_files() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "autumn.toml",
        "[rocksdb]\npath = \"file\"\ntimeout_ms = 5\n",
    );
    let env = env_for(dir.path())
        .with("AUTUMN_ROCKSDB__PATH", "env")
        .with("AUTUMN_ROCKSDB__TIMEOUT_MS", " 42 ")
        .with("AUTUMN_ROCKSDB__CACHE", "true")
        .with("AUTUMN_ROCKSDB__SESSIONS", "1")
        .with("AUTUMN_ROCKSDB__HEALTH_CHECK", "0")
        .with("AUTUMN_ROCKSDB__COLUMN_FAMILIES", "a, b,,c")
        .with("AUTUMN_ROCKSDB__MAX_OPEN_FILES", "-1")
        .with("AUTUMN_ROCKSDB__SESSION_TTL_SECS", "30")
        .with("AUTUMN_ROCKSDB__COMPRESSION", "none");
    let config = resolve(&env).unwrap();
    assert_eq!(config.path, "env");
    assert_eq!(config.timeout_ms, 42);
    assert!(config.cache);
    assert!(config.sessions);
    assert!(!config.health_check);
    assert_eq!(config.column_families, ["a", "b", "c"]);
    assert_eq!(config.max_open_files, Some(-1));
    assert_eq!(config.session_ttl_secs, Some(30));
    assert_eq!(config.compression, Compression::None);
}

#[test]
fn a_custom_section_has_its_own_variables() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path()).with("AUTUMN_MY_STORE__TIMEOUT_MS", "8");
    let config = RocksDbConfig::resolve_with_env("my-store", &env).unwrap();
    assert_eq!(config.timeout_ms, 8);
}

#[test]
fn bad_variables_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    for (key, value) in [
        ("AUTUMN_ROCKSDB__TIMEOUT_MS", "soon"),
        ("AUTUMN_ROCKSDB__TIMEOUT_MS", "-5"),
        ("AUTUMN_ROCKSDB__CACHE", "yes"),
        ("AUTUMN_ROCKSDB__MAX_OPEN_FILES", "many"),
    ] {
        let err = resolve(&env_for(dir.path()).with(key, value)).unwrap_err();
        assert!(err.to_string().contains(key), "{err}");
    }
}

#[test]
fn unknown_keys_and_bad_toml_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "autumn.toml", "[rocksdb]\npaht = \"x\"\n");
    let err = resolve(&env_for(dir.path())).unwrap_err();
    assert!(err.to_string().contains("paht"), "{err}");
    write(dir.path(), "autumn.toml", "[rocksdb\n");
    assert!(resolve(&env_for(dir.path())).is_err());
    write(dir.path(), "autumn.toml", "rocksdb = 3\n");
    let err = resolve(&env_for(dir.path())).unwrap_err();
    assert!(err.to_string().contains("must be a table"), "{err}");
}

#[test]
fn resolve_validates() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "autumn.toml", "[store]\ntimeout_ms = 0\n");
    let err = RocksDbConfig::resolve_with_env("store", &env_for(dir.path())).unwrap_err();
    assert!(err.to_string().contains("store.timeout_ms"), "{err}");
}

type Change = Box<dyn FnOnce(&mut RocksDbConfig)>;

fn assert_names(cases: Vec<(&str, Change)>) {
    for (key, change) in cases {
        let text = invalid(change);
        assert!(text.starts_with(key), "{key}: {text}");
    }
}

#[test]
fn validate_names_the_key() {
    let cases: Vec<(&str, Change)> = vec![
        ("rocksdb.path", Box::new(|c| c.path = " ".into())),
        (
            "rocksdb.access_mode",
            Box::new(|c| c.access_mode = AccessMode::ReadOnly),
        ),
        ("rocksdb.timeout_ms", Box::new(|c| c.timeout_ms = 0)),
        (
            "rocksdb.timeout_ms",
            Box::new(|c| c.timeout_ms = 86_400_001),
        ),
        (
            "rocksdb.max_concurrent_calls",
            Box::new(|c| c.max_concurrent_calls = 0),
        ),
        (
            "rocksdb.max_concurrent_calls",
            Box::new(|c| c.max_concurrent_calls = 1025),
        ),
        ("rocksdb.max_key_bytes", Box::new(|c| c.max_key_bytes = 0)),
        (
            "rocksdb.max_key_bytes",
            Box::new(|c| c.max_key_bytes = (1 << 20) + 1),
        ),
        (
            "rocksdb.max_value_bytes",
            Box::new(|c| c.max_value_bytes = 0),
        ),
        (
            "rocksdb.max_value_bytes",
            Box::new(|c| c.max_value_bytes = (1 << 30) + 1),
        ),
        (
            "rocksdb.max_scan_entries",
            Box::new(|c| c.max_scan_entries = 0),
        ),
        (
            "rocksdb.max_scan_entries",
            Box::new(|c| c.max_scan_entries = 100_001),
        ),
        ("rocksdb.max_scan_bytes", Box::new(|c| c.max_scan_bytes = 0)),
        (
            "rocksdb.session_ttl_secs",
            Box::new(|c| c.session_ttl_secs = Some(0)),
        ),
        (
            "rocksdb.block_cache_bytes",
            Box::new(|c| c.block_cache_bytes = Some(0)),
        ),
        (
            "rocksdb.write_buffer_bytes",
            Box::new(|c| c.write_buffer_bytes = Some(0)),
        ),
        (
            "rocksdb.max_open_files",
            Box::new(|c| c.max_open_files = Some(0)),
        ),
        (
            "rocksdb.max_open_files",
            Box::new(|c| c.max_open_files = Some(-2)),
        ),
        (
            "rocksdb.max_background_jobs",
            Box::new(|c| c.max_background_jobs = Some(0)),
        ),
        (
            "rocksdb.max_background_jobs",
            Box::new(|c| c.max_background_jobs = Some(257)),
        ),
        (
            "rocksdb.bloom_filter_bits",
            Box::new(|c| c.bloom_filter_bits = 65),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec![String::new()]),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec!["default".into()]),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec!["autumn_cache".into()]),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec!["a b".into()]),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec!["x".repeat(256)]),
        ),
        (
            "rocksdb.column_families",
            Box::new(|c| c.column_families = vec!["a".into(), "a".into()]),
        ),
    ];
    assert_names(cases);
}

#[test]
fn read_only_refuses_the_cache_and_sessions() {
    let read_only = |c: &mut RocksDbConfig| {
        c.path = "data".into();
        c.access_mode = AccessMode::ReadOnly;
    };
    assert!(
        invalid(|c| {
            read_only(c);
            c.cache = true;
        })
        .starts_with("rocksdb.cache")
    );
    assert!(
        invalid(|c| {
            read_only(c);
            c.sessions = true;
        })
        .starts_with("rocksdb.sessions")
    );
    let mut config = RocksDbConfig::default();
    read_only(&mut config);
    config.validate().unwrap();
}

#[test]
fn valid_edge_values_pass() {
    let mut config = RocksDbConfig::default();
    config.column_families = vec!["users".into(), "a-b.c_1".into()];
    config.max_open_files = Some(-1);
    config.bloom_filter_bits = 0;
    config.timeout_ms = 86_400_000;
    config.max_concurrent_calls = 1024;
    config.validate().unwrap();
}

#[test]
fn a_compression_that_is_not_built_in_is_an_error() {
    for compression in [Compression::Lz4, Compression::Zstd, Compression::Snappy] {
        let mut config = RocksDbConfig::default();
        config.compression = compression;
        assert_eq!(config.validate().is_ok(), compression.is_available());
    }
    assert!(Compression::None.is_available());
}

#[test]
fn session_ttl_prefers_the_plugin_value() {
    let mut config = RocksDbConfig::default();
    assert_eq!(config.session_ttl(600), Duration::from_secs(600));
    config.session_ttl_secs = Some(30);
    assert_eq!(config.session_ttl(600), Duration::from_secs(30));
}

#[test]
fn session_ttl_is_never_zero() {
    assert_eq!(
        RocksDbConfig::default().session_ttl(0),
        Duration::from_secs(1)
    );
}
