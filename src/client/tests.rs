#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::path::Path;

use serde::Deserialize;

use super::*;
use crate::config::AccessMode;
use crate::open::{CACHE_CF, SESSIONS_CF};

async fn memory() -> RocksDb {
    with(|_| {}).await
}

async fn with(change: impl FnOnce(&mut RocksDbConfig)) -> RocksDb {
    let mut config = RocksDbConfig::default();
    config.column_families = vec!["users".into()];
    change(&mut config);
    RocksDb::open(config).await.unwrap()
}

fn file_config(path: &Path) -> RocksDbConfig {
    let mut config = RocksDbConfig::default();
    config.path = path.to_str().unwrap().to_owned();
    config
}

async fn fill(db: &RocksDb, keys: &[&str]) {
    for key in keys {
        db.put(key, format!("v-{key}")).await.unwrap();
    }
}

fn keys(page: &Page) -> Vec<String> {
    page.entries
        .iter()
        .map(|e| String::from_utf8(e.key.clone()).unwrap())
        .collect()
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct User {
    name: String,
    age: u32,
}

#[tokio::test]
async fn put_get_exists_and_delete() {
    let db = memory().await;
    assert_eq!(db.get("k").await.unwrap(), None);
    assert!(!db.exists("k").await.unwrap());
    db.put("k", "v").await.unwrap();
    assert_eq!(db.get("k").await.unwrap(), Some(b"v".to_vec()));
    assert!(db.exists("k").await.unwrap());
    db.delete("k").await.unwrap();
    assert_eq!(db.get("k").await.unwrap(), None);
    db.delete("missing").await.unwrap();
}

#[tokio::test]
async fn json_values_round_trip() {
    let db = memory().await;
    let ada = User {
        name: "Ada".into(),
        age: 36,
    };
    db.put_json("user:1", &ada).await.unwrap();
    assert_eq!(db.get_json::<User>("user:1").await.unwrap(), Some(ada));
    assert_eq!(db.get_json::<User>("user:2").await.unwrap(), None);
}

#[tokio::test]
async fn a_value_that_is_not_json_gives_a_json_error() {
    let db = memory().await;
    db.put("k", "not json").await.unwrap();
    let err = db.get_json::<User>("k").await.unwrap_err();
    assert!(
        matches!(
            err,
            RocksDbError::Json {
                action: "decode",
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_value_that_does_not_encode_gives_a_json_error() {
    let db = memory().await;
    let map = std::collections::HashMap::from([((1, 2), 3)]);
    let err = db.put_json("k", &map).await.unwrap_err();
    assert!(
        matches!(
            err,
            RocksDbError::Json {
                action: "encode",
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn column_families_keep_data_apart() {
    let db = memory().await;
    let users = db.cf("users");
    assert_eq!(users.name(), "users");
    users.put("k", "user").await.unwrap();
    db.put("k", "default").await.unwrap();
    assert_eq!(users.get("k").await.unwrap(), Some(b"user".to_vec()));
    assert_eq!(db.get("k").await.unwrap(), Some(b"default".to_vec()));
    assert_eq!(
        db.cf("default").get("k").await.unwrap(),
        Some(b"default".to_vec())
    );
    assert_eq!(db.column_families(), ["default", "users"]);
}

#[tokio::test]
async fn unknown_and_reserved_column_families_are_refused() {
    let db = with(|c| c.cache = true).await;
    let err = db.cf("nothing").get("k").await.unwrap_err();
    assert_eq!(
        err,
        RocksDbError::UnknownColumnFamily {
            name: "nothing".into()
        }
    );
    for name in [CACHE_CF, SESSIONS_CF, "autumn_other"] {
        let err = db.cf(name).put("k", "v").await.unwrap_err();
        assert_eq!(
            err,
            RocksDbError::ReservedColumnFamily { name: name.into() }
        );
        let err = db
            .batch()
            .put_in(name, "k", "v")
            .commit()
            .await
            .unwrap_err();
        assert_eq!(
            err,
            RocksDbError::ReservedColumnFamily { name: name.into() }
        );
    }
    db.internal_cf(CACHE_CF).put("k", "v").await.unwrap();
}

#[tokio::test]
async fn size_limits_apply_before_the_call() {
    let db = with(|c| {
        c.max_key_bytes = 4;
        c.max_value_bytes = 8;
    })
    .await;
    db.put("1234", "12345678").await.unwrap();
    assert_eq!(
        db.put("12345", "v").await.unwrap_err(),
        RocksDbError::KeyTooLarge { size: 5, limit: 4 }
    );
    assert_eq!(
        db.put("k", "123456789").await.unwrap_err(),
        RocksDbError::ValueTooLarge { size: 9, limit: 8 }
    );
    assert!(matches!(
        db.get("12345").await,
        Err(RocksDbError::KeyTooLarge { .. })
    ));
    assert!(matches!(
        db.delete("12345").await,
        Err(RocksDbError::KeyTooLarge { .. })
    ));
    assert!(matches!(
        db.scan().prefix("12345").fetch().await,
        Err(RocksDbError::KeyTooLarge { .. })
    ));
    assert!(matches!(
        db.batch().put("k", "123456789").commit().await,
        Err(RocksDbError::ValueTooLarge { .. })
    ));
    assert!(matches!(
        db.put_json("k", "a long string value").await,
        Err(RocksDbError::ValueTooLarge { .. })
    ));
}

#[tokio::test]
async fn a_read_only_database_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("db"));
    let writer = RocksDb::open(config.clone()).await.unwrap();
    writer.put("k", "v").await.unwrap();
    writer.close().await;
    config.access_mode = AccessMode::ReadOnly;
    let reader = RocksDb::open(config).await.unwrap();
    assert_eq!(reader.get("k").await.unwrap(), Some(b"v".to_vec()));
    assert_eq!(
        reader.put("k", "w").await.unwrap_err(),
        RocksDbError::ReadOnly
    );
    assert_eq!(
        reader.delete("k").await.unwrap_err(),
        RocksDbError::ReadOnly
    );
    assert_eq!(
        reader.batch().delete("k").commit().await.unwrap_err(),
        RocksDbError::ReadOnly
    );
}

#[tokio::test]
async fn a_batch_writes_all_or_nothing() {
    let db = memory().await;
    db.put("gone", "x").await.unwrap();
    let batch = db
        .batch()
        .put("a", "1")
        .put_in("users", "b", "2")
        .delete("gone");
    assert_eq!(batch.len(), 3);
    assert!(!batch.is_empty());
    batch.commit().await.unwrap();
    assert_eq!(db.get("a").await.unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.cf("users").get("b").await.unwrap(), Some(b"2".to_vec()));
    assert_eq!(db.get("gone").await.unwrap(), None);

    let err = db
        .batch()
        .put("c", "3")
        .delete_in("nothing", "x")
        .commit()
        .await
        .unwrap_err();
    assert!(matches!(err, RocksDbError::UnknownColumnFamily { .. }));
    assert_eq!(db.get("c").await.unwrap(), None);
    db.batch().commit().await.unwrap();
}

#[tokio::test]
async fn a_scan_reads_keys_in_order() {
    let db = memory().await;
    fill(&db, &["b", "a", "c"]).await;
    let page = db.scan().fetch().await.unwrap();
    assert_eq!(keys(&page), ["a", "b", "c"]);
    assert_eq!(page.entries[0].value, b"v-a");
    assert_eq!(page.next, None);
}

#[tokio::test]
async fn a_scan_uses_the_prefix_and_the_range() {
    let db = memory().await;
    fill(&db, &["user:1", "user:2", "user:3", "userx", "order:1"]).await;
    let page = db.scan().prefix("user:").fetch().await.unwrap();
    assert_eq!(keys(&page), ["user:1", "user:2", "user:3"]);
    let page = db
        .scan()
        .start("user:2")
        .end("userx")
        .fetch()
        .await
        .unwrap();
    assert_eq!(keys(&page), ["user:2", "user:3"]);
    let page = db.scan().start("z").end("a").fetch().await.unwrap();
    assert!(page.entries.is_empty());
}

#[tokio::test]
async fn pages_give_a_cursor_until_the_end() {
    let db = memory().await;
    fill(&db, &["a", "b", "c", "d", "e"]).await;
    let first = db.scan().limit(2).fetch().await.unwrap();
    assert_eq!(keys(&first), ["a", "b"]);
    assert_eq!(first.next.as_deref(), Some(&b"b"[..]));
    let second = db
        .scan()
        .limit(2)
        .after(first.next.unwrap())
        .fetch()
        .await
        .unwrap();
    assert_eq!(keys(&second), ["c", "d"]);
    let third = db
        .scan()
        .limit(2)
        .after(second.next.unwrap())
        .fetch()
        .await
        .unwrap();
    assert_eq!(keys(&third), ["e"]);
    assert_eq!(third.next, None);
    let exact = db.scan().limit(5).fetch().await.unwrap();
    assert_eq!(exact.next, None);
}

#[tokio::test]
async fn the_byte_limit_ends_a_page_early() {
    let db = with(|c| c.max_scan_bytes = 10).await;
    fill(&db, &["a", "b", "c"]).await;
    // Each entry has 1 key byte and 3 value bytes.
    let page = db.scan().fetch().await.unwrap();
    assert_eq!(keys(&page), ["a", "b"]);
    assert_eq!(page.next.as_deref(), Some(&b"b"[..]));
}

#[tokio::test]
async fn one_entry_above_the_byte_limit_is_an_error() {
    let db = with(|c| c.max_scan_bytes = 3).await;
    fill(&db, &["a"]).await;
    assert_eq!(
        db.scan().fetch().await.unwrap_err(),
        RocksDbError::EntryTooLarge { limit_bytes: 3 }
    );
}

#[tokio::test]
async fn a_bad_scan_limit_is_an_error() {
    let db = with(|c| c.max_scan_entries = 5).await;
    for limit in [0, 6] {
        assert_eq!(
            db.scan().limit(limit).fetch().await.unwrap_err(),
            RocksDbError::ScanLimit { limit, max: 5 }
        );
    }
}

#[tokio::test]
async fn entries_decode_as_json() {
    let db = memory().await;
    db.put_json(
        "u",
        &User {
            name: "Bo".into(),
            age: 3,
        },
    )
    .await
    .unwrap();
    db.put("x", "{").await.unwrap();
    let page = db.scan().fetch().await.unwrap();
    assert_eq!(page.entries[0].value_as::<User>().unwrap().name, "Bo");
    assert!(matches!(
        page.entries[1].value_as::<User>(),
        Err(RocksDbError::Json { .. })
    ));
}

#[tokio::test]
async fn with_db_gives_the_full_api() {
    let db = memory().await;
    db.put("k", "v").await.unwrap();
    let value = db
        .with_db(|db| {
            let snapshot = db.snapshot();
            snapshot.get(b"k")
        })
        .await
        .unwrap();
    assert_eq!(value, Some(b"v".to_vec()));
    let err = db
        .with_db(|_| Database::open_default("/does/not/exist/\0bad").map(drop))
        .await
        .unwrap_err();
    assert!(matches!(err, RocksDbError::Database { .. }), "{err}");
}

#[tokio::test]
async fn a_panic_in_with_db_gives_task_failed() {
    let db = memory().await;
    let err = db
        .with_db(|_| -> Result<(), rocksdb::Error> { panic!("boom") })
        .await
        .unwrap_err();
    assert_eq!(err, RocksDbError::TaskFailed);
    db.put("k", "v").await.unwrap();
}

#[tokio::test]
async fn a_slow_call_times_out() {
    let db = with(|c| c.timeout_ms = 50).await;
    let start = std::time::Instant::now();
    let err = db
        .with_db(|_| {
            std::thread::sleep(Duration::from_secs(1));
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "the timeout is at once"
    );
    assert_eq!(
        err,
        RocksDbError::Timeout {
            timeout: Duration::from_millis(50)
        }
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn a_timed_out_call_keeps_its_slot_until_it_ends() {
    let db = with(|c| {
        c.timeout_ms = 50;
        c.max_concurrent_calls = 1;
    })
    .await;
    let slow = db.with_db(|_| {
        std::thread::sleep(Duration::from_millis(400));
        Ok(())
    });
    assert!(matches!(slow.await, Err(RocksDbError::Timeout { .. })));
    // The slow call still holds the only slot. The wait for it counts.
    assert!(matches!(
        db.get("k").await,
        Err(RocksDbError::Timeout { .. })
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(db.get("k").await.unwrap(), None);
}

#[tokio::test]
async fn metrics_count_calls_and_bytes() {
    let db = memory().await;
    db.put("key", "value").await.unwrap();
    db.get("key").await.unwrap();
    let _ = db.cf("nothing").get("k").await;
    let families = db.metrics().families();
    let find = |name: &str| {
        families
            .iter()
            .find(|f| f.name == name)
            .unwrap()
            .samples
            .clone()
    };
    // The check refuses the unknown column family before the call. The metrics do not count it.
    assert!((find("rocksdb_calls_started_total")[0].value - 2.0).abs() < f64::EPSILON);
    assert!((find("rocksdb_written_bytes_total")[0].value - 8.0).abs() < f64::EPSILON);
    assert!((find("rocksdb_read_bytes_total")[0].value - 8.0).abs() < f64::EPSILON);
    assert!((find("rocksdb_calls_open")[0].value).abs() < f64::EPSILON);
}

#[tokio::test]
async fn a_dropped_call_counts_as_cancelled() {
    let db = memory().await;
    let call = db.with_db(|_| {
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    });
    let _ = tokio::time::timeout(Duration::from_millis(10), call).await;
    let families = db.metrics().families();
    let calls = families
        .iter()
        .find(|f| f.name == "rocksdb_calls_total")
        .unwrap();
    let cancelled = calls
        .samples
        .iter()
        .find(|s| s.labels[0].1 == "cancelled")
        .unwrap();
    assert!((cancelled.value - 1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn shutdown_refuses_new_calls() {
    let db = memory().await;
    db.close().await;
    db.close().await;
    assert_eq!(db.get("k").await.unwrap_err(), RocksDbError::ShuttingDown);
    assert_eq!(
        db.put("k", "v").await.unwrap_err(),
        RocksDbError::ShuttingDown
    );
    assert!(db.database().is_none());
    assert!(db.properties().is_none());
}

#[tokio::test]
async fn shutdown_waits_for_open_calls() {
    use std::sync::atomic::AtomicBool;

    let db = memory().await;
    let started = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let slow = {
        let (db, started, done) = (db.clone(), Arc::clone(&started), Arc::clone(&done));
        tokio::spawn(async move {
            db.with_db(move |db| {
                started.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(200));
                db.put(b"late", b"v")?;
                done.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
        })
    };
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    db.close().await;
    assert!(
        done.load(Ordering::SeqCst),
        "the shutdown waited for the open call"
    );
    slow.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_flushes_and_closes_a_file_database() {
    let dir = tempfile::tempdir().unwrap();
    let config = file_config(&dir.path().join("db"));
    let db = RocksDb::open(config.clone()).await.unwrap();
    db.put("k", "v").await.unwrap();
    db.close().await;
    // The lock is free again, so the database is closed.
    let again = RocksDb::open(config).await.unwrap();
    assert_eq!(again.get("k").await.unwrap(), Some(b"v".to_vec()));
    let sst = again
        .property("rocksdb.total-sst-files-size")
        .await
        .unwrap();
    assert!(sst.unwrap() > 0, "the flush wrote a data file");
}

#[tokio::test]
async fn a_checkpoint_copies_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = RocksDb::open(file_config(&dir.path().join("db")))
        .await
        .unwrap();
    db.put("k", "v").await.unwrap();
    let copy = dir.path().join("copy");
    db.checkpoint(&copy).await.unwrap();
    assert!(db.checkpoint(&copy).await.is_err());
    let restored = RocksDb::open(file_config(&copy)).await.unwrap();
    assert_eq!(restored.get("k").await.unwrap(), Some(b"v".to_vec()));
}

#[tokio::test]
async fn an_in_memory_database_has_no_checkpoint() {
    let db = memory().await;
    assert_eq!(
        db.checkpoint("/tmp/never").await.unwrap_err(),
        RocksDbError::NotSupported {
            operation: "checkpoint"
        }
    );
}

#[tokio::test]
async fn properties_cover_each_column_family() {
    let db = memory().await;
    db.cf("users").put("k", "v").await.unwrap();
    let properties = db.properties().unwrap();
    assert_eq!(properties.background_errors, 0);
    let names: Vec<_> = properties
        .column_families
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["default", "users"]);
    assert!(properties.column_families[1].memtable_bytes > 0);
    assert_eq!(
        db.property("rocksdb.background-errors").await.unwrap(),
        Some(0)
    );
}

#[tokio::test]
async fn a_bad_config_does_not_open() {
    let mut config = RocksDbConfig::default();
    config.max_scan_entries = 0;
    assert!(matches!(
        RocksDb::open(config).await,
        Err(RocksDbError::Config(_))
    ));
}

#[tokio::test]
async fn debug_output_shows_no_keys_or_values() {
    let db = memory().await;
    let batch = db.batch().put("secret-key", "secret-value");
    let scan = db.scan().prefix("secret-prefix");
    let text = format!("{db:?} {:?} {batch:?} {scan:?}", db.cf("users"));
    assert!(!text.contains("secret"), "{text}");
    assert!(text.contains("users"), "{text}");
}

#[test]
fn a_timed_out_call_that_did_not_start_never_runs() {
    // One blocking thread: the second call waits in the queue until its deadline.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db = with(|c| c.timeout_ms = 100).await;
        let busy = db.with_db(|_| {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        let (busy, put) = tokio::join!(busy, db.put("k", "v"));
        assert!(matches!(busy, Err(RocksDbError::Timeout { .. })));
        assert!(matches!(put, Err(RocksDbError::Timeout { .. })));
        tokio::time::sleep(Duration::from_millis(800)).await;
        let db = db.clone();
        assert_eq!(
            db.get("k").await.unwrap(),
            None,
            "the queued write did not run"
        );
    });
}

#[tokio::test]
async fn an_empty_batch_after_close_fails() {
    let db = memory().await;
    db.close().await;
    assert_eq!(
        db.batch().commit().await.unwrap_err(),
        RocksDbError::ShuttingDown
    );
}

#[tokio::test]
async fn calls_after_close_are_not_counted() {
    let db = memory().await;
    db.close().await;
    let _ = db.get("k").await;
    let families = db.metrics().families();
    let started = families
        .iter()
        .find(|f| f.name == "rocksdb_calls_started_total")
        .unwrap();
    assert!(started.samples[0].value.abs() < f64::EPSILON);
}

fn sst_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "sst")
        })
        .count()
}

#[tokio::test]
async fn close_writes_a_data_file_before_it_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = RocksDb::open(file_config(&path)).await.unwrap();
    db.put("k", "v").await.unwrap();
    assert_eq!(sst_files(&path), 0);
    db.close().await;
    assert!(sst_files(&path) > 0, "the close flushed the memtable");
}

#[tokio::test]
async fn close_without_flush_on_shutdown_writes_no_data_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut config = file_config(&path);
    config.flush_on_shutdown = false;
    let db = RocksDb::open(config).await.unwrap();
    db.put("k", "v").await.unwrap();
    db.close().await;
    assert_eq!(sst_files(&path), 0);
}

#[tokio::test]
async fn flush_keeps_the_database_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = RocksDb::open(file_config(&path)).await.unwrap();
    db.put("k", "v").await.unwrap();
    db.flush().await.unwrap();
    assert!(sst_files(&path) > 0);
    assert_eq!(db.get("k").await.unwrap(), Some(b"v".to_vec()));
}
