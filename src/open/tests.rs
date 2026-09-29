#![allow(
    clippy::field_reassign_with_default,
    reason = "each test changes one key of the defaults"
)]

use std::path::Path;
use std::sync::Mutex;

use rocksdb::ErrorKind;

use super::*;
use crate::config::AccessMode;
use crate::envelope;

fn file_config(path: &Path) -> RocksDbConfig {
    let mut config = RocksDbConfig::default();
    config.path = path.to_str().unwrap().to_owned();
    config
}

fn cf<'a>(opened: &'a Opened, name: &str) -> &'a rocksdb::ColumnFamily {
    opened.db.cf_handle(name).unwrap()
}

#[test]
fn an_in_memory_database_opens_with_the_default_column_family() {
    let opened = open(&RocksDbConfig::default(), &[]).unwrap();
    assert_eq!(opened.column_families, ["default"]);
    opened.db.put(b"k", b"v").unwrap();
    assert_eq!(opened.db.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

#[test]
fn in_memory_databases_do_not_share_data() {
    let first = open(&RocksDbConfig::default(), &[]).unwrap();
    first.db.put(b"k", b"v").unwrap();
    let second = open(&RocksDbConfig::default(), &[]).unwrap();
    assert_eq!(second.db.get(b"k").unwrap(), None);
}

#[test]
fn configured_and_reserved_column_families_open() {
    let mut config = RocksDbConfig::default();
    config.column_families = vec!["users".into(), "orders".into()];
    config.cache = true;
    config.sessions = true;
    let opened = open(&config, &[]).unwrap();
    assert_eq!(opened.column_families[0], "default");
    for name in ["users", "orders", CACHE_CF, SESSIONS_CF] {
        assert!(opened.column_families.iter().any(|n| n == name), "{name}");
        opened.db.put_cf(cf(&opened, name), b"k", b"v").unwrap();
    }
}

#[test]
fn reserved_column_families_are_closed_when_not_in_use() {
    let opened = open(&RocksDbConfig::default(), &[]).unwrap();
    assert!(opened.db.cf_handle(CACHE_CF).is_none());
    assert!(opened.db.cf_handle(SESSIONS_CF).is_none());
}

#[test]
fn a_file_database_keeps_data_and_existing_column_families() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("db"));
    config.column_families = vec!["extra".into()];
    {
        let opened = open(&config, &[]).unwrap();
        opened.db.put_cf(cf(&opened, "extra"), b"k", b"v").unwrap();
    }
    config.column_families.clear();
    let opened = open(&config, &[]).unwrap();
    assert!(opened.column_families.iter().any(|n| n == "extra"));
    let value = opened.db.get_cf(cf(&opened, "extra"), b"k").unwrap();
    assert_eq!(value.as_deref(), Some(&b"v"[..]));
}

#[test]
fn a_missing_directory_without_create_if_missing_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("missing"));
    config.create_if_missing = false;
    let err = open(&config, &[]).err().unwrap();
    assert_eq!(err.kind(), Some(&ErrorKind::InvalidArgument));
}

#[test]
fn a_second_writer_on_the_same_path_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = file_config(&dir.path().join("db"));
    let _first = open(&config, &[]).unwrap();
    let err = open(&config, &[]).err().unwrap();
    assert_eq!(err.kind(), Some(&ErrorKind::IOError));
}

#[test]
fn a_read_only_open_reads_and_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("db"));
    config.column_families = vec!["users".into()];
    let writer = open(&config, &[]).unwrap();
    writer.db.put_cf(cf(&writer, "users"), b"k", b"v").unwrap();
    writer.db.flush_cf(cf(&writer, "users")).unwrap();
    config.access_mode = AccessMode::ReadOnly;
    let reader = open(&config, &[]).unwrap();
    let value = reader.db.get_cf(cf(&reader, "users"), b"k").unwrap();
    assert_eq!(value.as_deref(), Some(&b"v"[..]));
    assert!(reader.db.put(b"x", b"y").is_err());
}

#[test]
fn a_read_only_open_refuses_a_missing_column_family() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = file_config(&dir.path().join("db"));
    drop(open(&config, &[]).unwrap());
    config.access_mode = AccessMode::ReadOnly;
    config.column_families = vec!["users".into()];
    let err = open(&config, &[]).err().unwrap();
    assert_eq!(
        err,
        RocksDbError::UnknownColumnFamily {
            name: "users".into()
        }
    );
}

#[test]
fn a_bad_config_is_an_error() {
    let mut config = RocksDbConfig::default();
    config.timeout_ms = 0;
    assert!(matches!(open(&config, &[]), Err(RocksDbError::Config(_))));
}

#[test]
fn setup_hooks_run_in_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = Arc::clone(&order);
    let second = Arc::clone(&order);
    let setups: Vec<Setup> = vec![
        Arc::new(move |db: &Database| {
            first.lock().unwrap().push(1);
            db.put(b"seed", b"1")
        }),
        Arc::new(move |db: &Database| {
            second.lock().unwrap().push(2);
            assert_eq!(db.get(b"seed").unwrap().as_deref(), Some(&b"1"[..]));
            Ok(())
        }),
    ];
    open(&RocksDbConfig::default(), &setups).unwrap();
    assert_eq!(*order.lock().unwrap(), [1, 2]);
}

#[test]
fn a_failed_setup_hook_is_an_error() {
    // The open of a missing directory fails. It gives a real RocksDB error.
    let setups: Vec<Setup> = vec![Arc::new(|_: &Database| {
        Database::open(&rocksdb::Options::default(), "/does/not/exist").map(drop)
    })];
    assert!(matches!(
        open(&RocksDbConfig::default(), &setups),
        Err(RocksDbError::Database { .. })
    ));
}

#[test]
fn the_compaction_filter_removes_expired_envelopes() {
    let mut config = RocksDbConfig::default();
    config.cache = true;
    config.sessions = true;
    config.column_families = vec!["users".into()];
    let opened = open(&config, &[]).unwrap();
    let now = envelope::now_ms();
    for name in [CACHE_CF, SESSIONS_CF, "users"] {
        let handle = cf(&opened, name);
        let db = &opened.db;
        db.put_cf(handle, b"old", envelope::encode(b"x", Some(now - 1)))
            .unwrap();
        db.put_cf(
            handle,
            b"new",
            envelope::encode(b"x", Some(now + 3_600_000)),
        )
        .unwrap();
        db.put_cf(handle, b"forever", envelope::encode(b"x", None))
            .unwrap();
        db.flush_cf(handle).unwrap();
        db.compact_range_cf(handle, None::<&[u8]>, None::<&[u8]>);
        let expired_is_gone = db.get_cf(handle, b"old").unwrap().is_none();
        assert_eq!(expired_is_gone, name != "users", "{name}");
        assert!(db.get_cf(handle, b"new").unwrap().is_some(), "{name}");
        assert!(db.get_cf(handle, b"forever").unwrap().is_some(), "{name}");
    }
}
