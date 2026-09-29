#![allow(clippy::float_cmp, reason = "the counters are small whole numbers")]

use super::*;

fn sample(families: &[MetricFamily], name: &str, label: Option<(&str, &str)>) -> f64 {
    let family = families
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no family {name}"));
    family
        .samples
        .iter()
        .find(|s| label.is_none_or(|(k, v)| s.labels == vec![(k.to_owned(), v.to_owned())]))
        .unwrap_or_else(|| panic!("no sample {name} {label:?}"))
        .value
}

#[test]
fn counters_follow_the_calls() {
    let metrics = Metrics::default();
    for _ in 0..5 {
        metrics.started();
    }
    metrics.ended(Outcome::Succeeded);
    metrics.ended(Outcome::Failed);
    metrics.ended(Outcome::Cancelled);
    metrics.ended(Outcome::TimedOut);
    metrics.read(10);
    metrics.read(5);
    metrics.written(7);
    metrics.cache_hit();
    metrics.cache_hit();
    metrics.cache_miss();
    let families = metrics.families();
    assert_eq!(sample(&families, "rocksdb_calls_started_total", None), 5.0);
    for outcome in ["succeeded", "failed", "cancelled", "timed_out"] {
        let value = sample(&families, "rocksdb_calls_total", Some(("outcome", outcome)));
        assert_eq!(value, 1.0, "{outcome}");
    }
    assert_eq!(sample(&families, "rocksdb_calls_open", None), 1.0);
    assert_eq!(sample(&families, "rocksdb_read_bytes_total", None), 15.0);
    assert_eq!(sample(&families, "rocksdb_written_bytes_total", None), 7.0);
    let hits = sample(
        &families,
        "rocksdb_cache_requests_total",
        Some(("result", "hit")),
    );
    let misses = sample(
        &families,
        "rocksdb_cache_requests_total",
        Some(("result", "miss")),
    );
    assert_eq!((hits, misses), (2.0, 1.0));
}

#[test]
fn the_open_gauge_does_not_go_below_zero() {
    let metrics = Metrics::default();
    metrics.ended(Outcome::Succeeded);
    assert_eq!(sample(&metrics.families(), "rocksdb_calls_open", None), 0.0);
}

#[test]
fn property_families_have_a_sample_for_each_column_family() {
    let properties = Properties {
        background_errors: 2,
        column_families: vec![
            ColumnFamilyProperties {
                name: "default".into(),
                estimated_keys: 3,
                sst_files_bytes: 4,
                memtable_bytes: 5,
                pending_compaction_bytes: 6,
            },
            ColumnFamilyProperties {
                name: "users".into(),
                estimated_keys: 30,
                ..ColumnFamilyProperties::default()
            },
        ],
    };
    let families = property_families(&properties);
    assert_eq!(sample(&families, "rocksdb_background_errors", None), 2.0);
    let cf = |name| Some(("column_family", name));
    assert_eq!(
        sample(&families, "rocksdb_estimated_keys", cf("default")),
        3.0
    );
    assert_eq!(
        sample(&families, "rocksdb_estimated_keys", cf("users")),
        30.0
    );
    assert_eq!(
        sample(&families, "rocksdb_sst_files_bytes", cf("default")),
        4.0
    );
    assert_eq!(
        sample(&families, "rocksdb_memtable_bytes", cf("default")),
        5.0
    );
    assert_eq!(
        sample(&families, "rocksdb_pending_compaction_bytes", cf("default")),
        6.0
    );
}

#[test]
fn kinds_and_names_follow_the_rules() {
    let mut families = Metrics::default().families();
    families.extend(property_families(&Properties::default()));
    assert!(families.len() >= 11);
    for family in families {
        assert!(family.name.starts_with("rocksdb_"), "{}", family.name);
        assert!(!family.help.is_empty());
        let counter = family.name.ends_with("_total");
        assert_eq!(
            counter,
            matches!(family.kind, MetricKind::Counter),
            "{}",
            family.name
        );
    }
}
