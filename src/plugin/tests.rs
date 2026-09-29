#![allow(clippy::float_cmp, reason = "the counters are small whole numbers")]
#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::time::Duration;

use super::*;

#[test]
fn build_declares_the_config_section() {
    let app = autumn_web::app().plugin(RocksDbPlugin::new().config_section("store"));
    assert!(app.has_config_section("store"));
    assert!(!app.has_config_section("rocksdb"));
    assert!(app.has_plugin(PLUGIN_NAME));
}

#[test]
fn an_explicit_config_declares_no_section() {
    let app = autumn_web::app().plugin(RocksDbPlugin::new().config(RocksDbConfig::default()));
    assert!(!app.has_config_section("rocksdb"));
}

#[test]
fn resolve_applies_changes_then_validates() {
    let source = ConfigSource::Explicit(Box::default());
    let config = RocksDbPlugin::resolve(
        &source,
        vec![Box::new(|c: &mut RocksDbConfig| c.timeout_ms = 3)],
    )
    .unwrap();
    assert_eq!(config.timeout_ms, 3);
    let err = RocksDbPlugin::resolve(
        &source,
        vec![Box::new(|c: &mut RocksDbConfig| c.timeout_ms = 0)],
    )
    .unwrap_err();
    assert!(err.to_string().contains("rocksdb.timeout_ms"), "{err}");
}

#[test]
fn a_bad_section_names_the_section() {
    let source = ConfigSource::Section("store".to_owned());
    let err = RocksDbPlugin::resolve(
        &source,
        vec![Box::new(|c: &mut RocksDbConfig| c.timeout_ms = 0)],
    )
    .unwrap_err();
    assert!(err.to_string().contains("store.timeout_ms"), "{err}");
}

#[tokio::test]
async fn the_shutdown_hook_closes_the_handle() {
    let shared = Shared::default();
    let db = RocksDb::open(RocksDbConfig::default()).await.unwrap();
    assert!(shared.handle.set(db.clone()).is_ok());
    shared.shutdown().await;
    assert_eq!(db.get("k").await.unwrap_err(), RocksDbError::ShuttingDown);
}

#[tokio::test]
async fn shutdown_before_startup_does_nothing() {
    Shared::default().shutdown().await;
}

#[tokio::test]
async fn the_metrics_source_has_counters_and_properties() {
    let shared = Arc::new(Shared::default());
    let source = Source(Arc::clone(&shared));
    let names: Vec<String> = source.collect().into_iter().map(|f| f.name).collect();
    assert!(names.contains(&"rocksdb_calls_total".to_owned()));
    assert!(!names.contains(&"rocksdb_estimated_keys".to_owned()));

    let db = RocksDb::open_with(
        RocksDbConfig::default(),
        Vec::new(),
        Arc::clone(&shared.metrics),
    )
    .await
    .unwrap();
    db.put("k", "v").await.unwrap();
    assert!(shared.handle.set(db).is_ok());
    let families = source.collect();
    let calls = families
        .iter()
        .find(|f| f.name == "rocksdb_calls_started_total")
        .unwrap();
    assert_eq!(calls.samples[0].value, 1.0);
    assert!(families.iter().any(|f| f.name == "rocksdb_estimated_keys"));
}

#[tokio::test]
async fn the_watch_flushes_and_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = RocksDbConfig::default();
    config.path = dir.path().join("db").to_str().unwrap().to_owned();
    let state = AppState::for_test();
    let db = RocksDb::open(config).await.unwrap();
    db.put("k", "v").await.unwrap();
    let watch = tokio::spawn(watch_shutdown(state.clone(), db.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!watch.is_finished());
    state.begin_shutdown_for_test();
    tokio::time::timeout(Duration::from_secs(2), watch)
        .await
        .unwrap()
        .unwrap();
    // Autumn still drains requests after the mark. The database must serve them.
    assert_eq!(db.get("k").await.unwrap(), Some(b"v".to_vec()));
    let sst = db
        .with_db(|db| db.property_int_value("rocksdb.total-sst-files-size"))
        .await;
    assert!(sst.unwrap().unwrap() > 0, "the watch flushed the memtable");
}

#[test]
fn debug_shows_the_config_source() {
    let text = format!(
        "{:?}",
        RocksDbPlugin::new()
            .config_section("store")
            .setup(|_| Ok(()))
    );
    assert!(text.contains("store"), "{text}");
    assert!(text.contains("setups: 1"), "{text}");
}

#[tokio::test]
async fn the_metrics_source_after_close_has_counters_only() {
    let shared = Arc::new(Shared::default());
    let db = RocksDb::open_with(
        RocksDbConfig::default(),
        Vec::new(),
        Arc::clone(&shared.metrics),
    )
    .await
    .unwrap();
    assert!(shared.handle.set(db.clone()).is_ok());
    db.close().await;
    let names: Vec<String> = Source(shared)
        .collect()
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert!(names.contains(&"rocksdb_calls_total".to_owned()));
    assert!(!names.contains(&"rocksdb_estimated_keys".to_owned()));
}
