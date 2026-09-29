use super::*;

fn database(kind: ErrorKind) -> RocksDbError {
    RocksDbError::Database {
        kind,
        detail: "IO error: /secret/path/LOCK: busy".to_owned(),
    }
}

/// A real RocksDB error: the open of a missing directory without `create_if_missing`.
fn real_error() -> rocksdb::Error {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret-name");
    rocksdb::DB::open(&rocksdb::Options::default(), &path).unwrap_err()
}

#[test]
fn a_rocksdb_error_keeps_the_kind_and_the_message() {
    let err = RocksDbError::from(real_error());
    assert_eq!(err.kind(), Some(ErrorKind::InvalidArgument));
    assert!(err.detail().unwrap().contains("secret-name"));
}

#[test]
fn the_text_shows_the_kind_and_not_the_detail() {
    let err = RocksDbError::from(real_error());
    let text = err.to_string();
    assert_eq!(text, "RocksDB refused the call: invalid argument");
}

#[test]
fn a_json_error_hides_the_value() {
    let source = serde_json::from_str::<u32>("\"secret-value\"").unwrap_err();
    let err = RocksDbError::json("decode", &source);
    assert_eq!(err.to_string(), "the value does not decode as JSON");
    assert!(err.detail().unwrap().contains("secret-value"));
    assert_eq!(err.kind(), None);
}

#[test]
fn other_errors_have_no_detail_and_no_kind() {
    assert_eq!(RocksDbError::ShuttingDown.detail(), None);
    assert_eq!(RocksDbError::ShuttingDown.kind(), None);
}

#[test]
fn the_status_map() {
    let timeout = RocksDbError::Timeout {
        timeout: Duration::from_secs(1),
    };
    assert_eq!(timeout.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        RocksDbError::ShuttingDown.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    for kind in [
        ErrorKind::Busy,
        ErrorKind::TryAgain,
        ErrorKind::TimedOut,
        ErrorKind::ShutdownInProgress,
    ] {
        assert_eq!(database(kind).status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    let key = RocksDbError::KeyTooLarge { size: 2, limit: 1 };
    let value = RocksDbError::ValueTooLarge { size: 2, limit: 1 };
    assert_eq!(key.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(value.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let batch = RocksDbError::BatchTooLarge { size: 2, limit: 1 };
    assert_eq!(batch.status(), StatusCode::PAYLOAD_TOO_LARGE);
    for err in [
        database(ErrorKind::Corruption),
        database(ErrorKind::Io),
        RocksDbError::ReadOnly,
        RocksDbError::NotInstalled,
        RocksDbError::TaskFailed,
        RocksDbError::UnknownColumnFamily { name: "x".into() },
        RocksDbError::NotSupported {
            operation: "checkpoint",
        },
    ] {
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR, "{err}");
    }
}

#[test]
fn retryable_errors() {
    assert!(
        RocksDbError::Timeout {
            timeout: Duration::ZERO
        }
        .is_retryable()
    );
    for kind in [ErrorKind::Busy, ErrorKind::TryAgain, ErrorKind::TimedOut] {
        assert!(database(kind).is_retryable());
    }
    assert!(!database(ErrorKind::Corruption).is_retryable());
    assert!(!database(ErrorKind::ShutdownInProgress).is_retryable());
    assert!(!RocksDbError::ShuttingDown.is_retryable());
}

#[test]
fn into_autumn_keeps_the_status() {
    let err = RocksDbError::Timeout {
        timeout: Duration::ZERO,
    }
    .into_autumn();
    assert_eq!(err.status(), StatusCode::GATEWAY_TIMEOUT);
    let err: Result<(), _> = Err(RocksDbError::ShuttingDown);
    assert_eq!(
        err.or_http().unwrap_err().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn config_errors_convert() {
    let err = RocksDbError::from(ConfigError("rocksdb.path is bad".into()));
    assert_eq!(err.to_string(), "rocksdb.path is bad");
}

#[test]
fn not_supported_names_the_operation() {
    let err = RocksDbError::NotSupported {
        operation: "checkpoint",
    };
    assert_eq!(
        err.to_string(),
        "checkpoint is not supported for an in-memory database"
    );
}

#[test]
fn debug_output_hides_the_detail() {
    let err = RocksDbError::from(real_error());
    let text = format!("{err:?}");
    assert!(!text.contains("secret-name"), "{text}");
    assert!(text.contains("invalid argument"), "{text}");
    let source = serde_json::from_str::<u32>("\"secret-value\"").unwrap_err();
    let text = format!("{:?}", RocksDbError::json("decode", &source));
    assert!(!text.contains("secret-value"), "{text}");
}

#[test]
fn each_rocksdb_kind_maps_to_a_kind() {
    use rocksdb::ErrorKind as Rocks;
    let pairs = [
        (Rocks::NotFound, ErrorKind::NotFound),
        (Rocks::Corruption, ErrorKind::Corruption),
        (Rocks::NotSupported, ErrorKind::NotSupported),
        (Rocks::InvalidArgument, ErrorKind::InvalidArgument),
        (Rocks::IOError, ErrorKind::Io),
        (Rocks::MergeInProgress, ErrorKind::MergeInProgress),
        (Rocks::Incomplete, ErrorKind::Incomplete),
        (Rocks::ShutdownInProgress, ErrorKind::ShutdownInProgress),
        (Rocks::TimedOut, ErrorKind::TimedOut),
        (Rocks::Aborted, ErrorKind::Aborted),
        (Rocks::Busy, ErrorKind::Busy),
        (Rocks::Expired, ErrorKind::Expired),
        (Rocks::TryAgain, ErrorKind::TryAgain),
        (Rocks::CompactionTooLarge, ErrorKind::CompactionTooLarge),
        (Rocks::ColumnFamilyDropped, ErrorKind::ColumnFamilyDropped),
        (Rocks::Unknown, ErrorKind::Unknown),
    ];
    for (rocks, ours) in pairs {
        assert_eq!(ErrorKind::from(rocks), ours);
        assert!(!ours.to_string().is_empty());
    }
}

#[test]
fn a_bad_scan_limit_is_a_bad_request() {
    let err = RocksDbError::ScanLimit { limit: 0, max: 5 };
    assert_eq!(err.status(), StatusCode::BAD_REQUEST);
}
