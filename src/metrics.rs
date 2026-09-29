//! Call counters and RocksDB property gauges. The plugin module adds them to Autumn.
//!
//! # Contract
//!
//! - `rocksdb_calls_started_total` counts the calls that got past the shutdown check.
//! - `rocksdb_calls_total` counts the calls that ended, with the label `outcome`.
//! - `rocksdb_calls_open` is the calls that did not end yet. It never goes below zero.
//! - `rocksdb_read_bytes_total` and `rocksdb_written_bytes_total` count key and value bytes.
//! - `rocksdb_cache_requests_total` counts cache reads, with the label `result`: `hit` or `miss`.
//! - Property gauges have the label `column_family`. `rocksdb_background_errors` has no label.
//! - No metric name starts with `autumn_`. Each counter name ends with `_total`.

use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::actuator::{MetricFamily, MetricKind, MetricSample};

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

/// Counters for all calls of one database.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    started: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    timed_out: AtomicU64,
    open: AtomicU64,
    read_bytes: AtomicU64,
    written_bytes: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

/// The RocksDB properties of one column family.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ColumnFamilyProperties {
    pub(crate) name: String,
    pub(crate) estimated_keys: u64,
    pub(crate) sst_files_bytes: u64,
    pub(crate) memtable_bytes: u64,
    pub(crate) pending_compaction_bytes: u64,
}

/// The RocksDB properties of the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Properties {
    pub(crate) background_errors: u64,
    pub(crate) column_families: Vec<ColumnFamilyProperties>,
}

impl Metrics {
    pub(crate) fn started(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
        self.open.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn ended(&self, outcome: Outcome) {
        let counter = match outcome {
            Outcome::Succeeded => &self.succeeded,
            Outcome::Failed => &self.failed,
            Outcome::Cancelled => &self.cancelled,
            Outcome::TimedOut => &self.timed_out,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .open
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    pub(crate) fn read(&self, bytes: usize) {
        add(&self.read_bytes, bytes);
    }

    pub(crate) fn written(&self, bytes: usize) {
        add(&self.written_bytes, bytes);
    }

    pub(crate) fn cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// The counter families.
    pub(crate) fn families(&self) -> Vec<MetricFamily> {
        let load = |counter: &AtomicU64| value(counter.load(Ordering::Relaxed));
        vec![
            single(
                "rocksdb_calls_started_total",
                "RocksDB calls that the plugin started.",
                MetricKind::Counter,
                load(&self.started),
            ),
            family(
                "rocksdb_calls_total",
                "RocksDB calls that ended, by outcome.",
                MetricKind::Counter,
                [
                    ("outcome", "succeeded", load(&self.succeeded)),
                    ("outcome", "failed", load(&self.failed)),
                    ("outcome", "cancelled", load(&self.cancelled)),
                    ("outcome", "timed_out", load(&self.timed_out)),
                ],
            ),
            single(
                "rocksdb_calls_open",
                "RocksDB calls that did not end yet.",
                MetricKind::Gauge,
                load(&self.open),
            ),
            single(
                "rocksdb_read_bytes_total",
                "Key and value bytes that calls read.",
                MetricKind::Counter,
                load(&self.read_bytes),
            ),
            single(
                "rocksdb_written_bytes_total",
                "Key and value bytes that calls wrote.",
                MetricKind::Counter,
                load(&self.written_bytes),
            ),
            family(
                "rocksdb_cache_requests_total",
                "Cache reads, by result.",
                MetricKind::Counter,
                [
                    ("result", "hit", load(&self.cache_hits)),
                    ("result", "miss", load(&self.cache_misses)),
                ],
            ),
        ]
    }
}

/// The property families.
pub(crate) fn property_families(properties: &Properties) -> Vec<MetricFamily> {
    let per_cf = |name: &str, help: &str, get: fn(&ColumnFamilyProperties) -> u64| {
        family(
            name,
            help,
            MetricKind::Gauge,
            properties
                .column_families
                .iter()
                .map(|cf| ("column_family", cf.name.as_str(), value(get(cf)))),
        )
    };
    vec![
        single(
            "rocksdb_background_errors",
            "Background errors since the open. RocksDB never lowers this count.",
            MetricKind::Gauge,
            value(properties.background_errors),
        ),
        per_cf(
            "rocksdb_estimated_keys",
            "The estimated number of keys.",
            |cf| cf.estimated_keys,
        ),
        per_cf(
            "rocksdb_sst_files_bytes",
            "The size of all data files.",
            |cf| cf.sst_files_bytes,
        ),
        per_cf(
            "rocksdb_memtable_bytes",
            "The size of all memtables.",
            |cf| cf.memtable_bytes,
        ),
        per_cf(
            "rocksdb_pending_compaction_bytes",
            "The estimated bytes that compaction must rewrite.",
            |cf| cf.pending_compaction_bytes,
        ),
    ]
}

fn add(counter: &AtomicU64, bytes: usize) {
    counter.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
}

const fn value(raw: u64) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "Prometheus values are f64")]
    let value = raw as f64;
    value
}

fn single(name: &str, help: &str, kind: MetricKind, value: f64) -> MetricFamily {
    MetricFamily {
        name: name.to_owned(),
        help: help.to_owned(),
        kind,
        samples: vec![MetricSample {
            labels: Vec::new(),
            value,
        }],
    }
}

fn family<'a>(
    name: &str,
    help: &str,
    kind: MetricKind,
    samples: impl IntoIterator<Item = (&'a str, &'a str, f64)>,
) -> MetricFamily {
    MetricFamily {
        name: name.to_owned(),
        help: help.to_owned(),
        kind,
        samples: samples
            .into_iter()
            .map(|(key, label, value)| MetricSample {
                labels: vec![(key.to_owned(), label.to_owned())],
                value,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;
