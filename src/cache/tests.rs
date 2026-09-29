#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::cache::{FillLockStatus, get_cached, insert_cached};
use serde::{Deserialize, Serialize};

use super::*;
use crate::config::RocksDbConfig;
use crate::metrics::Metrics;

async fn cache_with(change: impl FnOnce(&mut RocksDbConfig)) -> (RocksCache, RocksDb) {
    let mut config = RocksDbConfig::default();
    config.cache = true;
    change(&mut config);
    let db = RocksDb::open(config).await.unwrap();
    (RocksCache::new(db.clone()).unwrap(), db)
}

async fn cache() -> RocksCache {
    cache_with(|_| {}).await.0
}

fn file_config(path: &Path) -> RocksDbConfig {
    let mut config = RocksDbConfig::default();
    config.path = path.to_str().unwrap().to_owned();
    config.cache = true;
    config
}

fn requests(metrics: &Metrics, result: &str) -> f64 {
    metrics
        .families()
        .into_iter()
        .find(|f| f.name == "rocksdb_cache_requests_total")
        .unwrap()
        .samples
        .into_iter()
        .find(|s| s.labels[0].1 == result)
        .unwrap()
        .value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    id: u32,
    name: String,
}

#[tokio::test]
async fn serde_values_round_trip() {
    let cache = cache().await;
    let item = Item {
        id: 1,
        name: "widget".into(),
    };
    insert_cached(&cache, "item:1", item.clone(), None);
    assert_eq!(get_cached::<Item>(&cache, "item:1"), Some(item));
    let raw = cache.get_value("item:1").unwrap();
    let bytes = raw.downcast_ref::<RawCacheBytes>().unwrap();
    assert_eq!(bytes.0, br#"{"id":1,"name":"widget"}"#);
    assert_eq!(get_cached::<Item>(&cache, "item:2"), None);
}

#[tokio::test]
async fn insert_value_stores_known_types_only() {
    let cache = cache().await;
    cache.insert_value("s", Arc::new(String::from("text")));
    cache.insert_value("i64", Arc::new(7_i64));
    cache.insert_value("i32", Arc::new(8_i32));
    cache.insert_value("raw", Arc::new(RawCacheBytes(b"[1]".to_vec())));
    cache.insert_value("other", Arc::new(1.5_f32));
    assert_eq!(get_cached::<String>(&cache, "s").as_deref(), Some("text"));
    assert_eq!(get_cached::<i64>(&cache, "i64"), Some(7));
    assert_eq!(get_cached::<i32>(&cache, "i32"), Some(8));
    assert_eq!(get_cached::<Vec<u8>>(&cache, "raw"), Some(vec![1]));
    assert!(cache.get_value("other").is_none());
}

#[tokio::test]
async fn an_entry_expires_after_the_ttl() {
    let cache = cache().await;
    insert_cached(&cache, "k", 1_u8, Some(Duration::from_millis(50)));
    assert_eq!(get_cached::<u8>(&cache, "k"), Some(1));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(get_cached::<u8>(&cache, "k"), None);
}

#[tokio::test]
async fn invalidate_and_clear_remove_entries() {
    let (cache, db) = cache_with(|c| c.column_families = vec!["users".into()]).await;
    db.cf("users").put("keep", "v").await.unwrap();
    for key in ["a", "b", "ü-key"] {
        insert_cached(&cache, key, 1_u8, None);
    }
    cache.invalidate("a");
    assert!(cache.get_value("a").is_none());
    assert!(cache.get_value("b").is_some());
    cache.clear();
    assert!(cache.get_value("b").is_none());
    assert!(cache.get_value("ü-key").is_none());
    assert_eq!(
        db.cf("users").get("keep").await.unwrap(),
        Some(b"v".to_vec())
    );
}

#[tokio::test]
async fn entries_above_the_size_limits_are_not_stored() {
    let (cache, _db) = cache_with(|c| {
        c.max_key_bytes = 4;
        c.max_value_bytes = 16;
    })
    .await;
    insert_cached(&cache, "long-key", 1_u8, None);
    assert!(cache.get_value("long-key").is_none());
    insert_cached(&cache, "k", "a value that is too long".to_owned(), None);
    assert!(cache.get_value("k").is_none());
    insert_cached(&cache, "k", "short".to_owned(), None);
    assert!(cache.get_value("k").is_some());
}

#[tokio::test]
async fn bad_envelopes_are_misses() {
    let (cache, db) = cache_with(|_| {}).await;
    db.internal_cf(CACHE_CF)
        .put("k", "not an envelope")
        .await
        .unwrap();
    assert!(cache.get_value("k").is_none());
}

#[tokio::test]
async fn reads_count_hits_and_misses() {
    let (cache, db) = cache_with(|_| {}).await;
    insert_cached(&cache, "k", 1_u8, None);
    let _ = cache.get_value("k");
    let _ = cache.get_value("k");
    let _ = cache.get_value("missing");
    assert!((requests(db.metrics(), "hit") - 2.0).abs() < f64::EPSILON);
    assert!((requests(db.metrics(), "miss") - 1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn a_shut_down_cache_misses_and_does_not_panic() {
    let (cache, db) = cache_with(|_| {}).await;
    insert_cached(&cache, "k", 1_u8, None);
    db.shutdown().await;
    assert!(cache.get_value("k").is_none());
    insert_cached(&cache, "k", 1_u8, None);
    cache.invalidate("k");
    cache.clear();
}

#[tokio::test]
async fn entries_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = file_config(&dir.path().join("db"));
    let db = RocksDb::open(config.clone()).await.unwrap();
    insert_cached(&RocksCache::new(db.clone()).unwrap(), "k", 5_u8, None);
    db.shutdown().await;
    let db = RocksDb::open(config).await.unwrap();
    assert_eq!(
        get_cached::<u8>(&RocksCache::new(db).unwrap(), "k"),
        Some(5)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_thread_runtime_works() {
    let cache = cache().await;
    insert_cached(&cache, "k", 3_u8, None);
    assert_eq!(get_cached::<u8>(&cache, "k"), Some(3));
}

#[test]
fn no_runtime_works() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let cache = runtime.block_on(cache());
    insert_cached(&cache, "k", 4_u8, None);
    assert_eq!(get_cached::<u8>(&cache, "k"), Some(4));
}

#[tokio::test]
async fn new_needs_the_cache_column_family() {
    let db = RocksDb::open(RocksDbConfig::default()).await.unwrap();
    assert_eq!(
        RocksCache::new(db).unwrap_err(),
        RocksDbError::UnknownColumnFamily {
            name: CACHE_CF.into()
        }
    );
}

#[tokio::test]
async fn get_or_compute_fills_once() {
    let cache: Arc<dyn Cache> = Arc::new(cache().await);
    let fills = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let fills = Arc::clone(&fills);
        let value: String =
            autumn_web::cache::get_or_compute(&cache, "rt", None, move || async move {
                fills.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>("filled".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(value, "filled");
    }
    assert_eq!(fills.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_fill_lock_is_not_supported() {
    let cache = cache().await;
    assert_eq!(
        cache.try_acquire_fill_lock("k", "t", Duration::from_secs(1)),
        FillLockStatus::Unsupported
    );
}

#[tokio::test]
async fn debug_shows_no_keys() {
    let cache = cache().await;
    insert_cached(&cache, "secret", 1_u8, None);
    assert!(!format!("{cache:?}").contains("secret"));
}
