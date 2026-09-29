#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::path::Path;

use super::*;
use crate::config::RocksDbConfig;
use crate::envelope;

const HOUR: Duration = Duration::from_secs(3600);

async fn db_with(change: impl FnOnce(&mut RocksDbConfig)) -> RocksDb {
    let mut config = RocksDbConfig::default();
    config.sessions = true;
    change(&mut config);
    RocksDb::open(config).await.unwrap()
}

async fn store() -> RocksSessionStore {
    RocksSessionStore::new(db_with(|_| {}).await, HOUR).unwrap()
}

fn data() -> HashMap<String, String> {
    HashMap::from([("user_id".to_owned(), "42".to_owned())])
}

fn file_config(path: &Path) -> RocksDbConfig {
    let mut config = RocksDbConfig::default();
    config.path = path.to_str().unwrap().to_owned();
    config.sessions = true;
    config
}

#[tokio::test]
async fn save_load_and_destroy() {
    let store = store().await;
    assert_eq!(store.load("id").await.unwrap(), None);
    store.save("id", data()).await.unwrap();
    assert_eq!(store.load("id").await.unwrap(), Some(data()));
    store.destroy("id").await.unwrap();
    assert_eq!(store.load("id").await.unwrap(), None);
    store.destroy("id").await.unwrap();
}

async fn stored_expiry(db: &RocksDb, id: &str) -> u64 {
    let raw = db
        .internal_cf(SESSIONS_CF)
        .get(session_key(id))
        .await
        .unwrap()
        .unwrap();
    envelope::decode(&raw).unwrap().expires_at.unwrap()
}

#[tokio::test]
async fn an_expired_session_is_none() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    let payload = br#"{"user_id":"42"}"#;
    let past = envelope::encode(payload, Some(envelope::now_ms() - 1));
    db.internal_cf(SESSIONS_CF)
        .put(session_key("id"), past)
        .await
        .unwrap();
    assert_eq!(store.load("id").await.unwrap(), None);
}

#[tokio::test]
async fn a_save_moves_the_expiry() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    let old = envelope::encode(b"{}", Some(envelope::now_ms() + 10));
    db.internal_cf(SESSIONS_CF)
        .put(session_key("id"), old)
        .await
        .unwrap();
    store.save("id", data()).await.unwrap();
    assert!(stored_expiry(&db, "id").await >= envelope::now_ms() + 3_500_000);
}

#[tokio::test]
async fn the_stored_value_is_an_envelope_with_the_expiry() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    let before = envelope::now_ms();
    store.save("id", data()).await.unwrap();
    let raw = db
        .internal_cf(SESSIONS_CF)
        .get(session_key("id"))
        .await
        .unwrap()
        .unwrap();
    let stored = envelope::decode(&raw).unwrap();
    let expires_at = stored.expires_at.unwrap();
    assert!(expires_at >= before + 3_600_000);
    assert!(expires_at <= envelope::now_ms() + 3_600_000);
    assert_eq!(stored.payload, br#"{"user_id":"42"}"#);
}

#[tokio::test]
async fn bad_data_gives_a_new_session() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    let sessions = db.internal_cf(SESSIONS_CF);
    sessions
        .put(session_key("raw"), "not an envelope")
        .await
        .unwrap();
    sessions
        .put(session_key("json"), envelope::encode(b"[1, 2]", None))
        .await
        .unwrap();
    assert_eq!(store.load("raw").await.unwrap(), None);
    assert_eq!(store.load("json").await.unwrap(), None);
}

#[tokio::test]
async fn a_long_id_gives_none_on_load_and_an_error_on_save() {
    let db = db_with(|c| c.max_key_bytes = 8).await;
    let store = RocksSessionStore::new(db, HOUR).unwrap();
    let id = "secret-session-id";
    assert_eq!(store.load(id).await.unwrap(), None);
    store.destroy(id).await.unwrap();
    let err = store.save(id, data()).await.unwrap_err();
    assert!(!err.to_string().contains(id), "{err}");
    assert!(err.to_string().contains("save"), "{err}");
}

#[tokio::test]
async fn an_unbound_store_gives_errors() {
    let store = RocksSessionStore::unbound();
    assert!(store.load("id").await.is_err());
    assert!(store.save("id", data()).await.is_err());
    let err = store.destroy("id").await.unwrap_err();
    assert!(err.to_string().contains("not ready"), "{err}");
    assert!(format!("{store:?}").contains("bound: false"));
}

#[tokio::test]
async fn a_bound_store_works_after_bind() {
    let store = RocksSessionStore::unbound();
    let copy = store.clone();
    store.bind(db_with(|_| {}).await, HOUR).unwrap();
    copy.save("id", data()).await.unwrap();
    assert_eq!(store.load("id").await.unwrap(), Some(data()));
}

#[tokio::test]
async fn a_shut_down_store_gives_errors() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    db.close().await;
    assert!(store.load("id").await.is_err());
    assert!(store.save("id", data()).await.is_err());
    assert!(store.destroy("id").await.is_err());
}

#[tokio::test]
async fn new_needs_the_sessions_column_family() {
    let db = RocksDb::open(RocksDbConfig::default()).await.unwrap();
    assert_eq!(
        RocksSessionStore::new(db, HOUR).unwrap_err(),
        RocksDbError::UnknownColumnFamily {
            name: SESSIONS_CF.into()
        }
    );
}

#[tokio::test]
async fn sessions_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = file_config(&dir.path().join("db"));
    let db = RocksDb::open(config.clone()).await.unwrap();
    RocksSessionStore::new(db.clone(), HOUR)
        .unwrap()
        .save("id", data())
        .await
        .unwrap();
    db.close().await;
    let store = RocksSessionStore::new(RocksDb::open(config).await.unwrap(), HOUR).unwrap();
    assert_eq!(store.load("id").await.unwrap(), Some(data()));
}

#[tokio::test]
async fn the_store_keeps_a_hash_of_the_id_and_not_the_id() {
    let db = db_with(|_| {}).await;
    let store = RocksSessionStore::new(db.clone(), HOUR).unwrap();
    store.save("secret-session-id", data()).await.unwrap();
    let keys = db
        .with_db(|db| {
            let cf = db.cf_handle(SESSIONS_CF).expect("open");
            db.iterator_cf(cf, rocksdb::IteratorMode::Start)
                .map(|item| item.map(|(key, _)| key.to_vec()))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .unwrap();
    assert_eq!(keys, [session_key("secret-session-id").to_vec()]);
    assert_eq!(keys[0].len(), 32);
}

#[test]
fn session_keys_are_sha256() {
    // SHA-256 of "abc", from FIPS 180-2.
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let hex = session_key("abc").iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    });
    assert_eq!(hex, expected);
}

#[tokio::test]
async fn a_read_only_database_refuses_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("db"));
    config.cache = true;
    RocksDb::open(config.clone()).await.unwrap().close().await;
    config.sessions = false;
    config.cache = false;
    config.access_mode = crate::config::AccessMode::ReadOnly;
    // The old reserved column families are open again, but read-only.
    let db = RocksDb::open(config).await.unwrap();
    assert!(db.column_families().iter().any(|n| n == SESSIONS_CF));
    assert_eq!(
        RocksSessionStore::new(db.clone(), HOUR).unwrap_err(),
        RocksDbError::ReadOnly
    );
    assert_eq!(
        crate::cache::RocksCache::new(db).unwrap_err(),
        RocksDbError::ReadOnly
    );
}
