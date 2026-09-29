#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

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
    assert_eq!(problem(0, 0), None);
    assert_eq!(problem(1, 0), Some("background errors"));
    assert_eq!(problem(0, 1), Some("writes stopped"));
    assert_eq!(problem(2, 1), Some("background errors"));
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
    db.shutdown().await;
    let output = DatabaseCheck::new(shared).check().await;
    assert_eq!(output.status, HealthStatus::Down);
    assert_eq!(output.details["state"], "shut down");
}
