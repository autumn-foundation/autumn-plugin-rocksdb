#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::time::{Duration, Instant};

use autumn_web::actuator::HealthStatus;

use super::*;
use crate::client::RocksDb;
use crate::config::RocksDbConfig;

async fn started(config: RocksDbConfig) -> (Arc<Shared>, RocksDb) {
    let shared = Arc::new(Shared::default());
    let db = RocksDb::open(config).await.unwrap();
    assert!(shared.handle.set(db.clone()).is_ok());
    (shared, db)
}

#[test]
fn problem_names_the_reason() {
    assert_eq!(problem(false, 0), None);
    assert_eq!(problem(true, 0), Some("background errors"));
    assert_eq!(problem(false, 1), Some("writes stopped"));
    assert_eq!(problem(true, 1), Some("background errors"));
}

#[test]
fn only_recent_background_errors_count() {
    let start = Instant::now();
    let mut tracker = ErrorTracker::default();
    assert!(!tracker.observe(0, start));
    assert!(tracker.observe(1, start + Duration::from_secs(1)));
    assert!(tracker.observe(1, start + Duration::from_secs(30)));
    // RocksDB recovered. The count never goes down, but it stops growing.
    assert!(!tracker.observe(1, start + Duration::from_secs(62)));
    assert!(tracker.observe(2, start + Duration::from_secs(70)));
}

#[test]
fn the_first_count_after_startup_is_recent() {
    let mut tracker = ErrorTracker::default();
    assert!(tracker.observe(3, Instant::now()));
}

#[test]
fn the_check_answers_before_the_autumn_timeout() {
    let check = DatabaseCheck::new(Arc::new(Shared::default()));
    assert!(CHECK_WAIT < Duration::from_millis(check.timeout_ms()));
}

#[tokio::test]
async fn a_check_before_startup_is_down() {
    let output = DatabaseCheck::new(Arc::new(Shared::default()))
        .check()
        .await;
    assert_eq!(output.status, HealthStatus::Down);
    assert_eq!(output.details["state"], "not started");
}

#[tokio::test]
async fn a_started_database_is_up() {
    let (shared, _db) = started(RocksDbConfig::default()).await;
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Up);
    assert_eq!(output.details["database"], "in-memory");
    assert_eq!(output.details["access_mode"], "read_write");
}

#[tokio::test]
async fn a_file_database_says_file_and_not_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = RocksDbConfig::default();
    config.path = dir.path().join("secret-name").to_str().unwrap().to_owned();
    let (shared, _db) = started(config).await;
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Up);
    assert_eq!(output.details["database"], "file");
    assert!(!format!("{:?}", output.details).contains("secret-name"));
}

#[tokio::test]
async fn a_check_after_shutdown_is_down() {
    let (shared, db) = started(RocksDbConfig::default()).await;
    db.close().await;
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Down);
    assert_eq!(output.details["state"], "shut down");
}

#[tokio::test]
async fn a_read_only_database_says_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = RocksDbConfig::default();
    config.path = dir.path().join("db").to_str().unwrap().to_owned();
    RocksDb::open(config.clone()).await.unwrap().close().await;
    config.access_mode = crate::config::AccessMode::ReadOnly;
    let (shared, _db) = started(config).await;
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.details["access_mode"], "read_only");
}

#[tokio::test]
async fn stopped_writes_are_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = RocksDbConfig::default();
    config.path = dir.path().join("db").to_str().unwrap().to_owned();
    let (shared, db) = started(config).await;
    db.with_db(|db| {
        db.set_options(&[
            ("level0_file_num_compaction_trigger", "1000"),
            ("level0_slowdown_writes_trigger", "1"),
            ("level0_stop_writes_trigger", "1"),
        ])?;
        db.put(b"k", b"v")?;
        db.flush()
    })
    .await
    .unwrap();
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Down);
    assert_eq!(output.details["state"], "writes stopped");
}

#[tokio::test]
async fn the_check_does_not_wait_for_a_slot() {
    let mut config = RocksDbConfig::default();
    config.max_concurrent_calls = 1;
    let (shared, db) = started(config).await;
    let busy = tokio::spawn(async move {
        db.with_db(|_| {
            std::thread::sleep(Duration::from_secs(1));
            Ok(())
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = Instant::now();
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Up);
    assert!(start.elapsed() < Duration::from_millis(500));
    busy.await.unwrap().unwrap();
}
