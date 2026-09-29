//! The plugin in an Autumn test app.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers panic on a broken setup"
)]
// `set_cache` sets a process-wide cache. Each `TestApp` build clears it, so tests do not
// see the cache of another test. Tests must not assert on `global_cache()`: it races.

use std::collections::HashMap;

use autumn_plugin_rocksdb::{
    RocksDb, RocksDbConfig, RocksDbError, RocksDbPlugin, RocksDbResultExt as _, RocksSessionStore,
};
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::session::SessionStore;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Note {
    text: String,
}

#[get("/notes/{id}")]
async fn note(db: RocksDb, Path(id): Path<String>) -> AutumnResult<Json<Note>> {
    let note = db
        .cf("notes")
        .get_json::<Note>(format!("note:{id}"))
        .await
        .or_http()?;
    note.map(Json)
        .ok_or_else(|| AutumnError::not_found_msg("no note"))
}

#[post("/notes/{id}")]
async fn add(db: RocksDb, Path(id): Path<String>) -> AutumnResult<&'static str> {
    db.cf("notes")
        .put_json(format!("note:{id}"), &Note { text: id.clone() })
        .await
        .or_http()?;
    Ok("ok")
}

#[post("/big")]
async fn big(db: RocksDb) -> AutumnResult<&'static str> {
    db.put("big", vec![0_u8; 100]).await.or_http()?;
    Ok("ok")
}

fn config() -> RocksDbConfig {
    let mut config = RocksDbConfig::default();
    config.column_families = vec!["notes".into()];
    config
}

fn plugin() -> RocksDbPlugin {
    RocksDbPlugin::new().config(config()).setup(|db| {
        let notes = db
            .cf_handle("notes")
            .expect("the notes column family is open");
        db.put_cf(notes, b"note:seed", br#"{"text":"seed"}"#)
    })
}

fn app(plugin: RocksDbPlugin) -> TestClient {
    TestApp::new()
        .routes(routes![note, add, big])
        .plugin(plugin)
        .build()
}

#[tokio::test]
async fn a_handler_uses_the_extractor() {
    let client = app(plugin());
    let response = client.get("/notes/seed").send().await;
    response.assert_ok();
    assert_eq!(
        response.json::<Note>(),
        Note {
            text: "seed".into()
        }
    );
    client.post("/notes/new").send().await.assert_ok();
    client.get("/notes/new").send().await.assert_ok();
    client.get("/notes/none").send().await.assert_status(404);
}

#[tokio::test]
async fn a_large_value_gives_payload_too_large() {
    let client = app(plugin().configure(|c| c.max_value_bytes = 10));
    client.post("/big").send().await.assert_status(413);
}

#[tokio::test]
async fn configure_changes_the_config() {
    let client = app(plugin().configure(|c| c.timeout_ms = 1234));
    let db = RocksDb::from_state(client.state()).unwrap();
    assert_eq!(db.config().timeout_ms, 1234);
}

#[tokio::test]
async fn the_extractor_fails_without_the_plugin() {
    let client = TestApp::new().routes(routes![note]).build();
    client.get("/notes/seed").send().await.assert_status(500);
}

#[tokio::test]
#[should_panic(expected = "Plugin startup hook failed")]
async fn a_bad_config_stops_the_boot() {
    let _ = app(RocksDbPlugin::new().configure(|c| c.max_concurrent_calls = 0));
}

#[tokio::test]
#[should_panic(expected = "Plugin startup hook failed")]
async fn a_failed_setup_hook_stops_the_boot() {
    let _ = app(RocksDbPlugin::new()
        .config(config())
        .setup(|_| autumn_plugin_rocksdb::Database::open_default("/does/not/\0exist").map(drop)));
}

#[tokio::test]
async fn the_cache_option_installs_the_app_cache() {
    let client = app(plugin().configure(|c| c.cache = true));
    let cache = client.state().cache().expect("the plugin installs a cache");
    autumn_web::cache::insert_cached(cache.as_ref(), "plugin-cache-key", 9_u8, None);
    let db = RocksDb::from_state(client.state()).unwrap();
    let raw = db
        .with_db(|db| {
            let cf = db.cf_handle(autumn_plugin_rocksdb::CACHE_CF).expect("open");
            db.get_cf(cf, b"plugin-cache-key")
        })
        .await
        .unwrap();
    assert!(raw.is_some(), "the entry is in the cache column family");
}

// `TestApp` uses its own memory session store for HTTP requests. So this test uses the
// store that the plugin binds and puts in the app state.
#[tokio::test]
async fn the_sessions_option_binds_a_session_store() {
    let client = app(plugin().configure(|c| {
        c.sessions = true;
        c.session_ttl_secs = Some(60);
    }));
    let store = client
        .state()
        .extension::<RocksSessionStore>()
        .expect("the plugin puts the bound store in the app state");
    let data = HashMap::from([("k".to_owned(), "v".to_owned())]);
    store.save("sid", data.clone()).await.unwrap();
    assert_eq!(store.load("sid").await.unwrap(), Some(data));
}

#[tokio::test]
async fn without_the_sessions_option_there_is_no_session_store() {
    let client = app(plugin());
    assert!(client.state().extension::<RocksSessionStore>().is_none());
}

#[tokio::test]
async fn the_shutdown_mark_keeps_the_database_open() {
    let client = app(plugin());
    client.state().begin_shutdown_for_test();
    // Autumn drains requests after the mark. Handlers must still work.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    client.get("/notes/seed").send().await.assert_ok();
    let db = RocksDb::from_state(client.state()).unwrap();
    assert_ne!(db.get("k").await, Err(RocksDbError::ShuttingDown));
}

fn app_with(config: AutumnConfig, plugin: RocksDbPlugin) -> TestClient {
    TestApp::new()
        .config(config)
        .routes(routes![note, add, big])
        .plugin(plugin)
        .build()
}

async fn session_expiry(client: &TestClient) -> u64 {
    let store = client
        .state()
        .extension::<RocksSessionStore>()
        .expect("the store is in the state");
    store
        .save("sid", HashMap::from([("k".to_owned(), "v".to_owned())]))
        .await
        .unwrap();
    let db = RocksDb::from_state(client.state()).unwrap();
    db.with_db(|db| {
        let cf = db
            .cf_handle(autumn_plugin_rocksdb::SESSIONS_CF)
            .expect("open");
        let mut found = None;
        for item in db.iterator_cf(cf, autumn_plugin_rocksdb::rocksdb::IteratorMode::Start) {
            let (_, value) = item?;
            let mut expiry = [0; 8];
            expiry.copy_from_slice(&value[1..9]);
            found = Some(u64::from_be_bytes(expiry));
        }
        Ok(found.expect("one session"))
    })
    .await
    .unwrap()
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[tokio::test]
async fn the_session_ttl_falls_back_to_the_autumn_max_age() {
    let mut config = AutumnConfig::default();
    config.session.max_age_secs = 7;
    let client = app_with(config, plugin().configure(|c| c.sessions = true));
    let before = now_ms();
    let expiry = session_expiry(&client).await;
    assert!(
        expiry >= before + 7_000 && expiry <= now_ms() + 7_000,
        "{expiry}"
    );
}

#[tokio::test]
async fn session_ttl_secs_wins_over_the_autumn_max_age() {
    let mut config = AutumnConfig::default();
    config.session.max_age_secs = 7;
    let client = app_with(
        config,
        plugin().configure(|c| {
            c.sessions = true;
            c.session_ttl_secs = Some(60);
        }),
    );
    let before = now_ms();
    let expiry = session_expiry(&client).await;
    assert!(
        expiry >= before + 60_000 && expiry <= now_ms() + 60_000,
        "{expiry}"
    );
}

#[tokio::test]
async fn health_check_false_adds_no_indicator() {
    let with = app(plugin());
    let body = with.get("/actuator/health").send().await.text();
    assert!(body.contains("rocksdb"), "{body}");
    let without = app(plugin().configure(|c| c.health_check = false));
    let body = without.get("/actuator/health").send().await.text();
    assert!(!body.contains("rocksdb"), "{body}");
}
