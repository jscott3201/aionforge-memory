//! selene-db's `metrics` feature is enabled (this crate's Cargo.toml), so the engine's
//! internal instrumentation emits through the same `metrics` 0.24 facade the rest of the
//! workspace records on. This proves that wiring is live end to end: a real store query
//! reaches a recorder as `selene.queries.total`. With the feature off, selene's helper
//! bodies compile to no-ops and nothing is emitted — so this is the regression guard that
//! keeps the one-line feature flag from being silently dropped in a later refactor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionforge_store::{BoundQuery, Store};
use jiff::Zoned;
use metrics::{
    Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};

fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

/// Per-name counter totals captured by [`CountingRecorder`].
type Totals = Arc<Mutex<HashMap<String, u64>>>;

/// A counter handle that folds every increment into the shared totals under its metric
/// name (labels collapse into the name bucket, which is all this test asserts on).
struct CountingCounter {
    name: String,
    totals: Totals,
}

impl CounterFn for CountingCounter {
    fn increment(&self, value: u64) {
        *self
            .totals
            .lock()
            .expect("totals lock")
            .entry(self.name.clone())
            .or_default() += value;
    }

    fn absolute(&self, value: u64) {
        let mut totals = self.totals.lock().expect("totals lock");
        let entry = totals.entry(self.name.clone()).or_default();
        *entry = (*entry).max(value);
    }
}

/// A minimal scoped recorder: counters fold into a name->total map; gauges and histograms
/// are no-ops (this test asserts only on the `selene.queries.total` counter).
struct CountingRecorder {
    totals: Totals,
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(CountingCounter {
            name: key.name().to_string(),
            totals: Arc::clone(&self.totals),
        }))
    }

    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

#[test]
fn selene_internal_metrics_reach_a_recorder_with_the_feature_on() {
    let totals: Totals = Arc::new(Mutex::new(HashMap::new()));
    let recorder = CountingRecorder {
        totals: Arc::clone(&totals),
    };

    // A scoped (thread-local) recorder keeps this hermetic: there is no process-global
    // singleton to install once and have conflict across the parallel test binary.
    metrics::with_local_recorder(&recorder, || {
        // Opening + migrating runs many DDL statements through the gql runtime, each of
        // which emits selene.queries.total; the explicit read is belt-and-suspenders.
        let store = Store::open_in_memory_migrated(&now()).expect("open and migrate");
        let query = BoundQuery::new("MATCH (f:Fact) RETURN count(f) AS n");
        store.execute(&query).expect("count query");
    });

    let totals = totals.lock().expect("totals lock");
    let queries = totals.get("selene.queries.total").copied().unwrap_or(0);
    assert!(
        queries > 0,
        "selene.queries.total must be emitted through the metrics facade with the feature \
         on; recorded counters: {totals:?}"
    );
}
