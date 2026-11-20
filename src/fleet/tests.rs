#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use autumn_web::config::{DatabaseConfig, DatabaseFleetConfig};
use autumn_web::db::fleet_replication::FleetReplication;
use autumn_web::db::fleet_tombstones::FleetTombstones;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::plugin_conformance::CheckStatus;
use autumn_web::plugin_conformance::fleet::{FleetFixture, run_fleet_conformance};
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;
use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};

use super::*;

/// The kit fixture. With `replicated`, generations go to a local directory.
struct Fixture {
    replicated: bool,
}

/// The fixture with no replica.
const PLAIN: Fixture = Fixture { replicated: false };

fn control_pool(dir: &Path) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .unwrap()
    .unwrap()
}

impl FleetFixture for Fixture {
    type Engine = RocksDbEngine;

    async fn fleet(
        &self,
        config: DatabaseFleetConfig,
    ) -> Result<DatabaseFleet<RocksDbEngine>, String> {
        let scratch = Path::new(&config.root)
            .parent()
            .ok_or("the root has no parent")?
            .to_path_buf();
        let engine = RocksDbEngine::new(RocksDbConfig::default())
            .resident_bytes(1024)
            .ship_every(Duration::from_hours(1));
        let fleet = DatabaseFleet::with_engine(&config, engine).map_err(|e| e.to_string())?;
        if self.replicated {
            let destination: Arc<dyn ReplicaDestination> = Arc::new(
                FileDestination::new(scratch.join("replicas")).map_err(|e| e.to_string())?,
            );
            let clock = Arc::new(autumn_web::time::SystemClock);
            let tombstones = Arc::new(FleetTombstones::new(
                control_pool(&scratch),
                Arc::clone(&clock) as _,
            ));
            let replication = Arc::new(
                FleetReplication::new(
                    destination,
                    ReplicationSettings {
                        database_path: PathBuf::new(),
                        root: "app/kit".to_owned(),
                        sync_interval: Duration::from_secs(1),
                        snapshot_interval: Duration::from_hours(1),
                        max_wal_bytes: 4 * 1024 * 1024,
                        retention: Duration::from_hours(24),
                        verify_interval: None,
                    },
                    Duration::from_secs(30),
                    false,
                    clock,
                )
                .with_tombstones(tombstones),
            );
            fleet
                .install_replication(replication)
                .map_err(|_| "replication is installed twice".to_owned())?;
        }
        Ok(fleet)
    }

    async fn restore(
        &self,
        fleet: &DatabaseFleet<RocksDbEngine>,
        key: &FleetDbKey,
    ) -> Option<Result<(), FleetError>> {
        if self.replicated {
            Some(fleet.restore_latest(key).await)
        } else {
            None
        }
    }

    async fn write(&self, handle: &RocksDb, value: &str) -> Result<(), String> {
        let value = value.to_owned();
        handle
            .with_db(move |db| {
                let n = db.iterator(rocksdb::IteratorMode::Start).count();
                db.put(format!("v{n:08}"), value.as_bytes())
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn read(&self, handle: &RocksDb) -> Result<Vec<String>, String> {
        handle
            .with_db(|db| {
                db.iterator(rocksdb::IteratorMode::Start)
                    .map(|item| item.map(|(_, v)| String::from_utf8_lossy(&v).into_owned()))
                    .collect()
            })
            .await
            .map_err(|e| e.to_string())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_passes_the_fleet_conformance_kit() {
    let scratch = tempfile::tempdir().unwrap();
    let report = run_fleet_conformance("rocksdb", &PLAIN, scratch.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replicated_engine_passes_every_check() {
    let scratch = tempfile::tempdir().unwrap();
    let fixture = Fixture { replicated: true };
    let report = run_fleet_conformance("rocksdb", &fixture, scratch.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for check in &report.checks {
        assert_eq!(
            check.status,
            CheckStatus::Pass,
            "{}: {}",
            check.name,
            check.message
        );
    }
}

fn fleet_config(root: &Path) -> DatabaseFleetConfig {
    let mut table = toml::Table::new();
    table.insert("mode".to_owned(), "tenant".into());
    table.insert("root".to_owned(), root.display().to_string().into());
    table.insert("create_on_demand".to_owned(), true.into());
    toml::Value::Table(table).try_into().unwrap()
}

/// The file names and the bytes of a snapshot.
fn contents(files: &[PathBuf]) -> Vec<(String, Vec<u8>)> {
    files
        .iter()
        .map(|f| {
            (
                f.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(f).unwrap(),
            )
        })
        .collect()
}

fn key(fleet: &DatabaseFleet<RocksDbEngine>, tenant: &str) -> FleetDbKey {
    fleet.key_for(tenant).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_database_snapshots_to_the_same_bytes() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet = PLAIN
        .fleet(fleet_config(&scratch.path().join("fleet")))
        .await
        .unwrap();
    let key = key(&fleet, "acme");
    let db = fleet.open(&key).await.unwrap();
    PLAIN.write(db.handle(), "one").await.unwrap();
    let path = fleet.path_of(&key);
    let snap = |n: &str| {
        let dir = scratch.path().join(n);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let (a, b) = (snap("a"), snap("b"));
    let first = fleet
        .engine()
        .snapshot(db.handle(), &path, &a)
        .await
        .unwrap();
    let second = fleet
        .engine()
        .snapshot(db.handle(), &path, &b)
        .await
        .unwrap();
    assert_eq!(contents(&first), contents(&second));
    let c = snap("c");
    PLAIN.write(db.handle(), "two").await.unwrap();
    let third = fleet
        .engine()
        .snapshot(db.handle(), &path, &c)
        .await
        .unwrap();
    assert_ne!(contents(&first), contents(&third));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_database_snapshots_its_file() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet = PLAIN
        .fleet(fleet_config(&scratch.path().join("fleet")))
        .await
        .unwrap();
    let key = key(&fleet, "acme");
    let db = fleet.open(&key).await.unwrap();
    PLAIN.write(db.handle(), "kept").await.unwrap();
    let handle = db.handle().clone();
    drop(db);
    fleet.close(&key).await;
    assert!(handle.is_closed());
    assert!(!fleet.engine().in_use(&handle));
    let dir = scratch.path().join("copy");
    std::fs::create_dir_all(&dir).unwrap();
    let files = fleet
        .engine()
        .snapshot(&handle, &fleet.path_of(&key), &dir)
        .await
        .unwrap();
    assert!(files.iter().all(|f| f.is_file()));
    assert!(files.iter().any(|f| f.ends_with("CURRENT")));
    assert!(!files.iter().any(|f| f.ends_with("LOCK")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_epoch_stamp_is_the_stamp_of_the_open() {
    let scratch = tempfile::tempdir().unwrap();
    let engine = RocksDbEngine::new(RocksDbConfig::default());
    let fleet =
        DatabaseFleet::with_engine(&fleet_config(&scratch.path().join("f")), engine).unwrap();
    let key = key(&fleet, "acme");
    let path = fleet.path_of(&key);
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), None);
    let token = FencingToken::try_from(7_i64).unwrap();
    let db = fleet.engine().open(&key, &path, Some(token)).await.unwrap();
    fleet.engine().close(&db);
    drop(db);
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), Some(7));
    let older = FencingToken::try_from(3_i64).unwrap();
    let err = fleet
        .engine()
        .open(&key, &path, Some(older))
        .await
        .unwrap_err();
    assert!(
        matches!(err, FleetError::NewerEpoch { found: 7, .. }),
        "{err}"
    );
    assert!(fleet.engine().remove(&key, &path).unwrap());
    assert!(!fleet.engine().remove(&key, &path).unwrap());
}

#[test]
fn a_database_with_no_identity_has_no_stamp() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("plain");
    rocksdb::DB::open_default(&path)
        .unwrap()
        .put(b"x", b"1")
        .unwrap();
    let engine = RocksDbEngine::new(RocksDbConfig::default());
    let fleet = DatabaseFleet::with_engine(&fleet_config(scratch.path()), engine).unwrap();
    let key = key(&fleet, "acme");
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), None);
    assert!(fleet.engine().exists(&path));
    assert!(!fleet.engine().exists(scratch.path()));
    assert_eq!(
        fleet
            .engine()
            .epoch_stamp(&key, &scratch.path().join("none"))
            .unwrap(),
        None
    );
}

#[test]
fn a_bad_database_gives_an_epoch_stamp_error() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("bad");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("CURRENT"), b"MANIFEST-000099\n").unwrap();
    let engine = RocksDbEngine::new(RocksDbConfig::default());
    let fleet = DatabaseFleet::with_engine(&fleet_config(scratch.path()), engine).unwrap();
    let key = key(&fleet, "acme");
    let err = fleet.engine().epoch_stamp(&key, &path).unwrap_err();
    assert!(matches!(err, FleetError::Io { .. }), "{err}");
}

#[test]
fn a_copy_skips_the_lock_and_the_logs() {
    assert!(skipped("LOCK") && skipped("LOG") && skipped("LOG.old.1700000000"));
    assert!(!skipped("CURRENT") && !skipped("000012.sst"));
}

struct MapEnv(HashMap<String, String>);

impl Env for MapEnv {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        self.0
            .get(key)
            .cloned()
            .ok_or(std::env::VarError::NotPresent)
    }
}

fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

#[test]
fn the_config_reads_the_fleet_and_the_engine_keys() {
    let config = RocksDbFleetConfig::from_table(
        "rocksdb_fleet",
        table(
            r#"
            mode = "tenant"
            root = "data/tenants"
            max_resident_bytes = 4096
            ship_every_secs = 5
            resident_bytes = 1024
            [engine]
            block_cache_bytes = 2048
            max_concurrent_calls = 3
            [ownership]
            owner = "host-a"
            "#,
        ),
    )
    .unwrap();
    assert_eq!(config.fleet.root, "data/tenants");
    assert_eq!(config.fleet.max_resident_bytes, 4096);
    assert_eq!(config.ship_every, Duration::from_secs(5));
    assert_eq!(config.resident_bytes, 1024);
    assert_eq!(config.engine.block_cache_bytes, Some(2048));
    assert_eq!(config.engine.max_concurrent_calls, 3);
    assert_eq!(config.fleet.ownership.unwrap().owner, "host-a");
    let engine = RocksDbFleetConfig::new(fleet_config(Path::new("x"))).engine();
    assert!(format!("{engine:?}").contains("resident_bytes"));
    assert_eq!(engine.ship_mode().every(), Some(DEFAULT_SHIP_EVERY));
}

#[test]
fn the_config_refuses_bad_values() {
    let cases = [
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\npath = \"x\"",
            "engine.path",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nengine = 3",
            "engine must be a table",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\nbogus = 1",
            "engine",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nship_every_secs = \"x\"",
            "ship_every_secs",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nship_every_secs = 0",
            "ship_every_secs",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nresident_bytes = -1",
            "resident_bytes",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nresident_bytes = 0",
            "resident_bytes",
        ),
        ("mode = \"tenant\"\nroot = \" \"", "root"),
        ("root = \"r\"", "fleet"),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\naccess_mode = \"read_only\"",
            "engine.access_mode",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\nmax_concurrent_calls = 0",
            "engine.max_concurrent_calls",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\ncache = true",
            "engine.cache",
        ),
    ];
    for (text, needle) in cases {
        let err = RocksDbFleetConfig::from_table("rocksdb_fleet", table(text)).unwrap_err();
        assert!(err.to_string().contains(needle), "{text}: {err}");
    }
}

#[test]
fn variables_set_the_root_and_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("autumn.toml"),
        "[rocksdb_fleet]\nmode = \"tenant\"\nroot = \"from-file\"\n",
    )
    .unwrap();
    let env = MapEnv(HashMap::from([
        (
            "AUTUMN_MANIFEST_DIR".to_owned(),
            dir.path().display().to_string(),
        ),
        (
            "AUTUMN_ROCKSDB_FLEET__ROOT".to_owned(),
            "from-env".to_owned(),
        ),
        (
            "AUTUMN_ROCKSDB_FLEET__OWNERSHIP__OWNER".to_owned(),
            "host-b".to_owned(),
        ),
    ]));
    let config = RocksDbFleetConfig::resolve_with_env("rocksdb_fleet", &env).unwrap();
    assert_eq!(config.fleet.root, "from-env");
    assert_eq!(config.fleet.ownership.unwrap().owner, "host-b");
}

#[test]
fn the_plugin_resolves_an_explicit_config() {
    let good = RocksDbFleetConfig::new(fleet_config(Path::new("data")));
    assert!(RocksDbFleetPlugin::resolve(&FleetSource::Explicit(Box::new(good.clone()))).is_ok());
    let mut bad = good;
    bad.resident_bytes = 0;
    assert!(RocksDbFleetPlugin::resolve(&FleetSource::Explicit(Box::new(bad))).is_err());
    let plugin = RocksDbFleetPlugin::default()
        .config_section("tenants")
        .setup(|_| Ok(()));
    let text = format!("{plugin:?}");
    assert!(
        text.contains("tenants") && text.contains("setups: 1"),
        "{text}"
    );
    assert_eq!(plugin.name(), FLEET_PLUGIN_NAME);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_closes_every_database_at_shutdown() {
    let scratch = tempfile::tempdir().unwrap();
    let config = RocksDbFleetConfig::new(fleet_config(&scratch.path().join("fleet")));
    let state = AppState::for_test();
    let runtime = start(&state, &config, config.engine()).await.unwrap();
    let lease = FleetRocksDb::for_key(&state, "acme").await.unwrap();
    lease.put("k", "v").await.unwrap();
    let handle = (*lease).clone();
    assert_eq!(runtime.fleet.open_keys().len(), 1);
    drop(lease);
    runtime.shutdown().await;
    assert!(handle.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn ownership_needs_a_store() {
    let scratch = tempfile::tempdir().unwrap();
    let mut table = toml::Table::new();
    table.insert("mode".to_owned(), "tenant".into());
    table.insert(
        "root".to_owned(),
        scratch.path().display().to_string().into(),
    );
    let mut ownership = toml::Table::new();
    ownership.insert("owner".to_owned(), "host-a".into());
    table.insert("ownership".to_owned(), ownership.into());
    let config = RocksDbFleetConfig::new(toml::Value::Table(table).try_into().unwrap());
    let state = AppState::for_test();
    let err = start(&state, &config, config.engine()).await.err().unwrap();
    assert!(err.contains("ownership"), "{err}");
}
